//! External API (§2.2, §19): the Unix socket (MessagePack frames; UI and CLI), the
//! WebSocket JSON endpoint and HTTP static server (web patches, player page), and OSC.

pub mod auth;
pub mod client;
pub mod osc;

pub use auth::{Auth, Scope};

use anyhow::{Context, Result};
use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::response::IntoResponse;
use axum::routing::get;
use client::{ClientCfg, serve};
use futures_util::{SinkExt, StreamExt};
use se_hub::Hub;
use se_proto::Origin;
use se_proto::wire::{ClientMsg, ServerMsg, async_io};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::net::UnixListener;
use tokio::sync::mpsc;

/// Serve the Unix socket (0600 in a 0700 directory). Removes a stale socket first.
pub async fn serve_unix(hub: Arc<Hub>, auth: Arc<Auth>, path: PathBuf) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    if path.exists() {
        // refuse to steal a live engine's socket
        if std::os::unix::net::UnixStream::connect(&path).is_ok() {
            anyhow::bail!("another engine is listening on {}", path.display());
        }
        std::fs::remove_file(&path)?;
    }
    let listener = UnixListener::bind(&path).with_context(|| format!("bind {}", path.display()))?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    tracing::info!("api: unix socket {}", path.display());
    loop {
        let (stream, _) = listener.accept().await?;
        let (hub, auth) = (hub.clone(), auth.clone());
        tokio::spawn(async move {
            let (mut rd, mut wr) = stream.into_split();
            let (in_tx, in_rx) = mpsc::channel::<ClientMsg>(256);
            let (out_tx, mut out_rx) = mpsc::channel::<ServerMsg>(4096);
            let writer = tokio::spawn(async move {
                while let Some(m) = out_rx.recv().await {
                    if async_io::write_frame(&mut wr, &m).await.is_err() {
                        break;
                    }
                }
            });
            let reader = tokio::spawn(async move {
                while let Ok(m) = async_io::read_frame::<_, ClientMsg>(&mut rd).await {
                    if in_tx.send(m).await.is_err() {
                        break;
                    }
                }
            });
            serve(hub, auth, ClientCfg { trusted: true, default_origin: Origin::Ui, name: "unix".into() }, in_rx, out_tx).await;
            reader.abort();
            writer.abort();
        });
    }
}

#[derive(Clone)]
pub struct HttpState {
    pub hub: Arc<Hub>,
    pub auth: Arc<Auth>,
}

async fn ws_handler(ws: WebSocketUpgrade, Query(q): Query<HashMap<String, String>>, State(st): State<HttpState>) -> impl IntoResponse {
    let pre = q.get("token").cloned();
    ws.max_message_size(8 * 1024 * 1024).on_upgrade(move |socket| ws_client(socket, st, pre))
}

async fn ws_client(socket: WebSocket, st: HttpState, pre_token: Option<String>) {
    let (mut tx, mut rx) = socket.split();
    let (in_tx, in_rx) = mpsc::channel::<ClientMsg>(256);
    let (out_tx, mut out_rx) = mpsc::channel::<ServerMsg>(4096);
    if let Some(t) = pre_token {
        let _ = in_tx.send(ClientMsg::Hello { client: "ws".into(), token: Some(t), version: 1 }).await;
    }
    let writer = tokio::spawn(async move {
        while let Some(m) = out_rx.recv().await {
            let Ok(txt) = serde_json::to_string(&m) else { continue };
            if tx.send(Message::Text(txt.into())).await.is_err() {
                break;
            }
        }
    });
    let reader = tokio::spawn(async move {
        while let Some(Ok(m)) = rx.next().await {
            let msg = match m {
                Message::Text(t) => serde_json::from_str::<ClientMsg>(&t),
                Message::Binary(b) => serde_json::from_slice::<ClientMsg>(&b),
                Message::Close(_) => break,
                _ => continue,
            };
            match msg {
                Ok(m) => {
                    if in_tx.send(m).await.is_err() {
                        break;
                    }
                }
                Err(e) => tracing::debug!("ws: bad message: {e}"),
            }
        }
    });
    serve(st.hub, st.auth, ClientCfg { trusted: false, default_origin: Origin::Api, name: "ws".into() }, in_rx, out_tx).await;
    reader.abort();
    writer.abort();
}

/// HTTP server: `/ws` (JSON API), `/web/*` and `/engine.js` (web client), `/health`, plus any
/// extra routes (web patches, player page, overlays) from subsystems.
pub fn router(st: HttpState, web_root: &Path, extra: Router) -> Router {
    let web = tower_http::services::ServeDir::new(web_root);
    Router::new()
        .route("/ws", get(ws_handler))
        .route("/health", get(|| async { axum::Json(serde_json::json!({ "ok": true })) }))
        .route_service("/engine.js", tower_http::services::ServeFile::new(web_root.join("engine.js")))
        .nest_service("/web", web)
        .with_state(st)
        .merge(extra)
}

pub async fn serve_http(bind: SocketAddr, app: Router) -> Result<()> {
    let l = tokio::net::TcpListener::bind(bind).await.with_context(|| format!("bind {bind}"))?;
    tracing::info!("api: http/ws on http://{bind}");
    axum::serve(l, app).await?;
    Ok(())
}
