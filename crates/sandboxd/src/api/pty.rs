//! Interactive PTY endpoint (`GET /v1/sandboxes/:id/pty`, WebSocket). Bridges a
//! client WebSocket to an in-sandbox shell session (spec §19, §20 "open PTY").
//! `POST /v1/sandboxes/:id/pty-token` mints a one-time token for that route.

use crate::api::load_owned;
use crate::auth::AuthContext;
use crate::error::{ApiError, ApiResult};
use crate::runtime::PtySession;
use crate::state::AppState;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

/// Mint a one-time terminal token for this sandbox, so a relay can open its PTY
/// without holding the org's API key.
pub async fn mint_token(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let sb = load_owned(&state, &ctx, &id)?;
    let (token, expires_at) = state
        .terminal_tokens
        .mint(&sb.id, &sb.org_id, chrono::Utc::now())
        .ok_or_else(|| ApiError::Rejected("too many open terminal tokens".into()))?;
    Ok(Json(json!({"token": token, "expires_at": expires_at})))
}

pub async fn pty_ws(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(id): Path<String>,
    ws: WebSocketUpgrade,
) -> Response {
    let sb = match load_owned(&state, &ctx, &id) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    if !sb.state.is_active() {
        return ApiError::Conflict(format!("sandbox is {}", sb.state.as_str())).into_response();
    }
    let handle = match sb.runtime_handle.clone() {
        Some(h) => h,
        None => return ApiError::Conflict("no runtime handle".into()).into_response(),
    };
    if sb.node_id.as_deref() != Some(state.local_node_id.as_str()) {
        return ApiError::BadRequest(
            "PTY is only available for sandboxes on the local node".into(),
        )
        .into_response();
    }
    // An interactive PTY session counts as activity (review #6).
    state
        .store
        .touch_last_active(&sb.id, chrono::Utc::now())
        .ok();
    ws.on_upgrade(move |socket| run_pty(state, handle, socket))
}

async fn run_pty(state: AppState, handle: String, socket: WebSocket) {
    let session = match state.local.open_pty(&handle).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "open_pty failed");
            return;
        }
    };
    let PtySession {
        mut input,
        output,
        stderr,
        mut child,
    } = session;
    let (mut cl_tx, mut cl_rx) = socket.split();
    let (out_tx, mut out_rx) = mpsc::channel::<Vec<u8>>(128);

    // Pump terminal output (and the dev runtime's separate stderr) to the client.
    spawn_reader(output, out_tx.clone());
    if let Some(err) = stderr {
        spawn_reader(err, out_tx);
    }

    let sender = tokio::spawn(async move {
        while let Some(bytes) = out_rx.recv().await {
            if cl_tx.send(Message::Binary(bytes)).await.is_err() {
                break;
            }
        }
    });

    // Client input -> shell stdin.
    while let Some(Ok(msg)) = cl_rx.next().await {
        let bytes = match msg {
            Message::Text(t) => t.into_bytes(),
            Message::Binary(b) => b,
            Message::Close(_) => break,
            _ => continue,
        };
        if input.write_all(&bytes).await.is_err() {
            break;
        }
        let _ = input.flush().await;
    }

    if let Some(child) = child.as_mut() {
        let _ = child.kill().await;
    }
    sender.abort();
}

fn spawn_reader<R>(mut reader: R, tx: mpsc::Sender<Vec<u8>>)
where
    R: AsyncReadExt + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).await.is_err() {
                        break;
                    }
                }
            }
        }
    });
}
