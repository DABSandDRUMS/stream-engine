//! Session bookkeeping: event log, 20 Hz signal history, markers, and rotation (a session is
//! one stream run: it closes when the show returns to `offline`).

use crate::daemon::{Ctx, SessionMsg};
use se_hub::Bus;
use se_proto::{Op, Value};
use se_store::session::{LogRec, new_session_id};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

pub fn start(ctx: &Ctx) {
    // events + rotation
    {
        let ctx = ctx.clone();
        tokio::spawn(async move {
            let mut bus = ctx.hub.subscribe();
            let mode_generation = Arc::new(AtomicU64::new(0));
            loop {
                let b = match bus.recv().await {
                    Ok(b) => b,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!("session log lagged {n} bus messages");
                        continue;
                    }
                    Err(_) => break,
                };
                let Bus::Event(e) = &*b else { continue };
                let _ = ctx.log_tx.send(SessionMsg::Rec(LogRec::Ev { event: e.clone() }));
                if e.ty == "mode.changed" {
                    let generation = mode_generation.fetch_add(1, Ordering::Relaxed) + 1;
                    let from = e.payload.get_path("from").and_then(Value::as_str).unwrap_or("");
                    let to = e.payload.get_path("to").and_then(Value::as_str).unwrap_or("");
                    if to == "offline" && from != "offline" {
                        let ctx = ctx.clone();
                        let mode_generation = mode_generation.clone();
                        tokio::spawn(async move {
                            // OBS closes the file and reports its final path/track layout after
                            // the mode transition. Keep this session open until that metadata
                            // arrives; never block the event logger while waiting for an encoder.
                            for _ in 0..60 {
                                if mode_generation.load(Ordering::Relaxed) != generation {
                                    return;
                                }
                                if !ctx.hub.snapshot.load().bool("obs.record.active") {
                                    break;
                                }
                                tokio::time::sleep(Duration::from_millis(500)).await;
                            }
                            tokio::time::sleep(Duration::from_secs(2)).await;
                            if mode_generation.load(Ordering::Relaxed) == generation && ctx.hub.snapshot.load().str("show.mode") == Some("offline") {
                                rotate(&ctx).await;
                            }
                        });
                    }
                }
            }
        });
    }
    // markers
    {
        let ctx = ctx.clone();
        let mut rx = ctx.hub.route_actions("session");
        tokio::spawn(async move {
            while let Some(c) = rx.recv().await {
                let Op::Action { name, args } = &c.op else { continue };
                match name.as_str() {
                    "session.marker" => {
                        let label = args.get_path("label").or_else(|| args.get_path("args.0")).map(|v| v.to_string()).unwrap_or_else(|| "marker".into());
                        let m = Value::map()
                            .with("ts", se_clock::now() as i64)
                            .with("wall_ms", (se_clock::wall_now_ns() / 1_000_000) as i64)
                            .with("label", label)
                            .with("origin", c.origin.as_str())
                            .with("args", args.clone());
                        let _ = ctx.log_tx.send(SessionMsg::Marker(m.clone()));
                        ctx.hub.emit(se_proto::Event::new("session.marker", c.origin, m).with_causal(c.causal));
                    }
                    "session.meta" => {
                        if let (Some(k), Some(v)) = (args.get_path("key").and_then(Value::as_str), args.get_path("value")) {
                            let _ = ctx.log_tx.send(SessionMsg::Meta(k.to_string(), v.clone()));
                        }
                    }
                    "session.rotate" => rotate(&ctx).await,
                    other => tracing::warn!("unknown session action {other}"),
                }
            }
        });
    }
    // signal history at 20 Hz
    {
        let ctx = ctx.clone();
        tokio::spawn(async move {
            let mut t = tokio::time::interval(Duration::from_millis(50));
            loop {
                t.tick().await;
                let s = ctx.hub.snapshot.load();
                if s.signals.is_empty() {
                    continue;
                }
                let _ = ctx.log_tx.send(SessionMsg::Signals(s.now, s.signal_names.clone(), s.signals.clone()));
            }
        });
    }
}

async fn rotate(ctx: &Ctx) {
    let old = ctx.session.lock().clone();
    let new_id = new_session_id();
    if new_id == old {
        return;
    }
    let tick = ctx.hub.with_core(|c| Value::Int(c.tick_index() as i64)).await.as_i64().unwrap_or(0) as u64;
    let period = ctx.hub.with_core(|c| Value::Int(c.period() as i64)).await.as_i64().unwrap_or(4_166_666) as u64;
    let rs = ctx.hub.runtime().await;
    let _ = ctx.db.session_close(&old);
    let dir = ctx.project.root().join("sessions").join(&new_id);
    let _ = ctx.db.session_open(&new_id, &dir.to_string_lossy());
    let start = LogRec::Start { t0: ctx.t0, period, tick, wall_ns: se_clock::wall_now_ns() as i64, restore: rs };
    let _ = ctx.log_tx.send(SessionMsg::Rotate { new_id: new_id.clone(), start });
    *ctx.session.lock() = new_id.clone();
    ctx.hub.info.write().session = new_id.clone();
    ctx.hub.emit(se_proto::Event::new("session.closed", se_proto::Origin::System, Value::map().with("session", old.clone()).with("next", new_id.clone())));
    tracing::info!("session {old} closed; now {new_id}");
}
