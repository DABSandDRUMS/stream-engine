//! Session bookkeeping: event log, 20 Hz signal history, markers, and rotation (a session is
//! one stream run: it closes when the show returns to `offline`).

use crate::daemon::{Ctx, SessionMsg};
use se_proto::Event;
use se_proto::{Op, Value};
use se_store::session::{LogRec, new_session_id};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

pub fn start(ctx: &Ctx, mut events: tokio::sync::mpsc::UnboundedReceiver<Event>) {
    {
        let tx = ctx.log_tx.clone();
        ctx.hub.register_query("session.persist", Arc::new(move |_, args| {
            let tx = tx.clone();
            Box::pin(async move {
                let session = args.get_path("session").and_then(Value::as_str).ok_or("session.persist needs session")?.to_owned();
                let values = args.get_path("values").cloned().ok_or("session.persist needs values")?;
                let (done, wait) = tokio::sync::oneshot::channel();
                tx.send(SessionMsg::Persist { session, values, done }).map_err(|_| "session writer stopped".to_string())?;
                wait.await.map_err(|_| "session writer dropped acknowledgement".to_string())??;
                Ok(Value::Bool(true))
            })
        }));
    }
    // Events + rotation use a dedicated lossless channel. The public broadcast bus drops
    // messages under load; logging its lag warning into that same bus amplifies the overload.
    {
        let ctx = ctx.clone();
        tokio::spawn(async move {
            let mode_generation = Arc::new(AtomicU64::new(0));
            while let Some(e) = events.recv().await {
                let _ = ctx.log_tx.send(SessionMsg::Rec(LogRec::Ev { event: e.clone() }));
                if e.ty == "mode.changed" {
                    let generation = mode_generation.fetch_add(1, Ordering::Relaxed) + 1;
                    let from = e.payload.get_path("from").and_then(Value::as_str).unwrap_or("");
                    let to = e.payload.get_path("to").and_then(Value::as_str).unwrap_or("");
                    if to == "offline" && from != "offline" {
                        let ctx = ctx.clone();
                        let mode_generation = mode_generation.clone();
                        tokio::spawn(async move {
                            rotate(&ctx, Some((&mode_generation, generation))).await;
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
                    "session.rotate" => rotate(&ctx, None).await,
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

async fn finalize_recording(ctx: &Ctx) -> bool {
    if ctx.hub.snapshot.load().get("recording.active").is_none() {
        return true; // recording subsystem disabled
    }
    match ctx.hub.query("recording.finalize", Value::Null).await {
        Ok(_) => true,
        Err(e) => {
            tracing::error!("session kept open: recording finalization failed: {e}");
            false
        }
    }
}

async fn rotate(ctx: &Ctx, expected_mode: Option<(&AtomicU64, u64)>) {
    if !finalize_recording(ctx).await {
        return;
    }
    if expected_mode.is_some_and(|(counter, expected)| {
        counter.load(Ordering::Relaxed) != expected || ctx.hub.snapshot.load().str("show.mode") != Some("offline")
    }) {
        return;
    }
    let old = ctx.session.lock().clone();
    let new_id = new_session_id();
    if new_id == old {
        return;
    }
    let tick = ctx.hub.with_core(|c| Value::Int(c.tick_index() as i64)).await.as_i64().unwrap_or(0) as u64;
    let period = ctx.hub.with_core(|c| Value::Int(c.period() as i64)).await.as_i64().unwrap_or(4_166_666) as u64;
    let rs = ctx.hub.runtime().await;
    let start = LogRec::Start { t0: ctx.t0, period, tick, wall_ns: se_clock::wall_now_ns() as i64, restore: rs };
    let (done, wait) = tokio::sync::oneshot::channel();
    if ctx.log_tx.send(SessionMsg::Rotate { new_id: new_id.clone(), start, done }).is_err() {
        tracing::error!("session writer stopped before rotation");
        return;
    }
    if let Err(e) = wait.await.map_err(|e| e.to_string()).and_then(|result| result) {
        tracing::error!("session rotation failed: {e}");
        return;
    }
    let _ = ctx.db.session_close(&old);
    let dir = ctx.project.root().join("sessions").join(&new_id);
    let _ = ctx.db.session_open(&new_id, &dir.to_string_lossy());
    *ctx.session.lock() = new_id.clone();
    ctx.hub.info.write().session = new_id.clone();
    ctx.hub.emit(se_proto::Event::new("session.closed", se_proto::Origin::System, Value::map().with("session", old.clone()).with("next", new_id.clone())));
    tracing::info!("session {old} closed; now {new_id}");
}
