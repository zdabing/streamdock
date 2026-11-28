use std::sync::Arc;

use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::response::Response;
use futures_util::StreamExt;
use serde::Deserialize;

use crate::state::ServerState;

#[derive(Deserialize)]
pub struct WsAuthQuery {
    #[serde(default)]
    pub token: Option<String>,
}

pub async fn ws_handler(
    State(state): State<Arc<ServerState>>,
    Query(query): Query<WsAuthQuery>,
    ws: WebSocketUpgrade,
) -> Response {
    let token = query.token.unwrap_or_default();
    ws.on_upgrade(move |socket| handle_socket(state, token, socket))
}

async fn handle_socket(state: Arc<ServerState>, token: String, mut socket: WebSocket) {
    if !state.authorized(&token) {
        let _ = socket
            .send(Message::Close(Some(CloseFrame {
                code: 1008,
                reason: "Unauthorized".into(),
            })))
            .await;
        let _ = futures_util::SinkExt::close(&mut socket).await;
        return;
    }

    let initial = state.initial_ws_frames().await;
    for frame in initial {
        if socket.send(Message::Text(frame.into())).await.is_err() {
            return;
        }
    }

    let mut rx = state.ws_tx.subscribe();
    let shutdown = state.shutdown_cancel.clone();

    loop {
        tokio::select! {
            _ = shutdown.cancelled() => {
                let _ = futures_util::SinkExt::close(&mut socket).await;
                break;
            }
            recv = rx.recv() => {
                match recv {
                    Ok(msg) => {
                        if socket.send(Message::Text((*msg).clone().into())).await.is_err() {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
            incoming = socket.next() => {
                match incoming {
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                    Some(Ok(Message::Ping(payload))) => {
                        if socket.send(Message::Pong(payload)).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(_)) => {}
                }
            }
        }
    }
}
