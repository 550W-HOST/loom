//! WebSocket surface for UI clients.
//!
//! The client protocol is intentionally tiny and scoped:
//!
//! ```json
//! // client -> server
//! {"type":"subscribe","scope":{"kind":"thread","id":"thr_1"}}
//! {"type":"unsubscribe","scope":{"kind":"thread","id":"thr_1"}}
//! {"type":"ping"}
//!
//! // server -> client
//! {"type":"welcome","connection_id":1,"protocol_version":1}
//! {"type":"subscribed","scope":{"kind":"thread","id":"thr_1"},"first_subscriber":true}
//! {"type":"unsubscribed","scope":{"kind":"thread","id":"thr_1"}}
//! {"type":"event","event_id":"...","scope":{...},"payload":"{...}","created_at_ms":1}
//! {"type":"pong"}
//! {"type":"error","message":"..."}
//! ```
//!
//! Subscribing to a thread is the only thing a UI does to start receiving its
//! timeline: the same scope a producer published to. No handler is involved,
//! and the socket never learns what a thread is.

use axum::extract::ws::{Message, WebSocket};
use axum::extract::{State, WebSocketUpgrade};
use axum::response::Response;
use futures_util::{SinkExt, StreamExt};
use loom_relay::scope::Scope;

use crate::protocol::{ClientCommand, ServerMessage};
use crate::state::AppState;
use crate::transport::ChannelTransport;

/// Upgrades an HTTP request to a client WebSocket.
pub async fn client_socket(upgrade: WebSocketUpgrade, State(state): State<AppState>) -> Response {
    upgrade.on_upgrade(move |socket| handle_client(socket, state))
}

async fn handle_client(socket: WebSocket, state: AppState) {
    let (mut sink, mut stream) = socket.split();
    let (transport, mut outbound) = ChannelTransport::with_default_capacity();

    let Ok(connection_id) = state.hub.connect(Box::new(transport), Scope::Global).await else {
        return;
    };

    if send(
        &mut sink,
        &ServerMessage::Welcome {
            connection_id,
            protocol_version: crate::PROTOCOL_VERSION,
        },
    )
    .await
    .is_err()
    {
        let _ = state.hub.disconnect(connection_id).await;
        return;
    }

    // Outbound frames are written by their own task. The hub therefore never
    // awaits a socket: a client that stops reading fills its queue and then
    // gets dropped frames, rather than stalling the fan-out.
    let writer = tokio::spawn(async move {
        while let Some(frame) = outbound.recv().await {
            if sink
                .send(Message::Text(
                    String::from_utf8_lossy(&frame).into_owned().into(),
                ))
                .await
                .is_err()
            {
                break;
            }
        }
    });

    while let Some(message) = stream.next().await {
        let Ok(message) = message else {
            break;
        };
        let text = match message {
            Message::Text(text) => text,
            Message::Binary(bytes) => match String::from_utf8(bytes.to_vec()) {
                Ok(text) => text.into(),
                Err(_) => continue,
            },
            Message::Close(_) => break,
            Message::Ping(_) | Message::Pong(_) => continue,
        };

        let reply = match serde_json::from_str::<ClientCommand>(&text) {
            Ok(command) => handle_command(connection_id, command, &state).await,
            Err(error) => Some(ServerMessage::Error {
                message: format!("unrecognised command: {error}"),
            }),
        };

        if let Some(reply) = reply {
            let encoded = match serde_json::to_string(&reply) {
                Ok(encoded) => encoded,
                Err(_) => continue,
            };
            // Write control replies through the same queue as events so
            // ordering between an ack and the events it enables is preserved.
            let _ = state.hub.send_to(connection_id, encoded).await;
        }
    }

    let _ = state.hub.disconnect(connection_id).await;
    writer.abort();
}

async fn handle_command(
    connection_id: u64,
    command: ClientCommand,
    state: &AppState,
) -> Option<ServerMessage> {
    match command {
        ClientCommand::Subscribe { scope } => {
            let outcome = state.hub.subscribe(connection_id, scope.clone()).await;
            match outcome {
                Ok(outcome) => Some(ServerMessage::Subscribed {
                    scope,
                    first_subscriber: outcome.first_subscriber,
                }),
                Err(_) => Some(ServerMessage::Error {
                    message: "hub unavailable".into(),
                }),
            }
        }
        ClientCommand::Unsubscribe { scope } => {
            // Handled by the hub actor so the room table stays single-owner.
            let removed = state
                .hub
                .unsubscribe(connection_id, scope.clone())
                .await
                .unwrap_or(false);
            Some(if removed {
                ServerMessage::Unsubscribed { scope }
            } else {
                ServerMessage::Error {
                    message: format!("not subscribed to {scope}"),
                }
            })
        }
        ClientCommand::Ping => Some(ServerMessage::Pong),
    }
}

async fn send(
    sink: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    message: &ServerMessage,
) -> Result<(), ()> {
    let encoded = serde_json::to_string(message).map_err(|_| ())?;
    sink.send(Message::Text(encoded.into()))
        .await
        .map_err(|_| ())
}
