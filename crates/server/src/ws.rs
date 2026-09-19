//! WebSocket surface for public clients and workers.
//!
//! The public client protocol is intentionally tiny and typed. A UI says:
//!
//! ```json
//! // client -> server
//! {"type":"subscribe","target":{"kind":"thread-detail","threadId":"thr_1"}}
//! {"type":"unsubscribe","target":{"kind":"thread-detail","threadId":"thr_1"}}
//! {"type":"ping"}
//! ```
//!
//! A worker uses `/internal/ws` to enroll, then follows its own `host:{id}`
//! room:
//!
//! ```json
//! // worker -> server (`/internal/ws`)
//! {"type":"enroll_host","name":"laptop"}
//! {"type":"host_heartbeat","host_id":"host_..."}
//! {"type":"host_disconnect","host_id":"host_..."}
//!
//! // server -> worker
//! {"type":"hello","protocol_version":3}
//! {"type":"host_enrolled","host":{...},"event_id":"01M..."}
//! {"type":"host_heartbeat_ack","host_id":"host_...","last_seen_at_ms":1}
//! {"type":"host_disconnected","host_id":"host_..."}
//! {"type":"event","event_id":"...","scope":{...},"payload":"{...}","created_at_ms":1}
//! {"type":"pong"}
//! {"type":"error","message":"..."}
//! ```
//!
//! Public clients receive projected `changed` messages. The connection listens
//! to the relay's complete event stream, then filters those messages against
//! its typed targets so newly-created entities and narrow detail updates do
//! not depend on a relay room existing before the subscription.

use std::collections::HashSet;

use axum::extract::ws::{Message, WebSocket};
use axum::extract::{State, WebSocketUpgrade};
use axum::http::header::SEC_WEBSOCKET_PROTOCOL;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use futures_util::{SinkExt, StreamExt};
use loom_domain::HostId;
use loom_relay::event_id::EventId;
use loom_relay::now_ms;
use loom_relay::scope::Scope;
use tokio::sync::broadcast;

use crate::environments::EnvironmentReportOutcome;
use crate::interactions::RecordOutcome;
use crate::protocol::{
    public_messages_from_frame, ClientMessage, PublicEntity, ServerMessage as PublicServerMessage,
    ThreadChangeMetadata, WorkerClientMessage as ClientCommand,
    WorkerServerMessage as ServerMessage,
};
use crate::pump::PublicRealtimeEvent;
use crate::runs::ReportOutcome;
use crate::state::AppState;
use crate::transport::ChannelTransport;
use crate::PUBLIC_WS_SUBPROTOCOL;

/// Upgrades an HTTP request to the public bb WebSocket.
///
/// Product clients explicitly offer [`PUBLIC_WS_SUBPROTOCOL`]. A connection
/// without a subprotocol is an old v2 worker: it receives only the legacy
/// version-mismatch frame needed to enter self-update, then the socket closes.
/// No request is classified by `Origin` or user agent.
pub async fn client_socket(
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
    State(state): State<AppState>,
) -> Response {
    match requested_public_protocol(&headers) {
        Ok(true) => upgrade
            .protocols([PUBLIC_WS_SUBPROTOCOL])
            .on_upgrade(move |socket| handle_public_client(socket, state)),
        Ok(false) => upgrade.on_upgrade(handle_legacy_worker),
        Err(()) => StatusCode::BAD_REQUEST.into_response(),
    }
}

fn requested_public_protocol(headers: &HeaderMap) -> Result<bool, ()> {
    let Some(value) = headers.get(SEC_WEBSOCKET_PROTOCOL) else {
        return Ok(false);
    };
    let value = value.to_str().map_err(|_| ())?;
    if value
        .split(',')
        .map(str::trim)
        .any(|protocol| protocol == PUBLIC_WS_SUBPROTOCOL)
    {
        Ok(true)
    } else {
        Err(())
    }
}

/// Gives a deployed v2 worker the mismatch it needs to self-update.
async fn handle_legacy_worker(mut socket: WebSocket) {
    let welcome = serde_json::json!({
        "type": "welcome",
        "connection_id": 0,
        "protocol_version": crate::PROTOCOL_VERSION,
    });
    let _ = socket.send(Message::Text(welcome.to_string().into())).await;
    let _ = socket.send(Message::Close(None)).await;
}

/// Upgrades an HTTP request to the versioned worker WebSocket.
pub async fn worker_socket(upgrade: WebSocketUpgrade, State(state): State<AppState>) -> Response {
    upgrade.on_upgrade(move |socket| handle_worker(socket, state))
}

async fn handle_public_client(mut socket: WebSocket, state: AppState) {
    let mut events = state.public_events.subscribe();
    let mut targets = HashSet::new();

    loop {
        tokio::select! {
            incoming = socket.recv() => {
                let Some(Ok(message)) = incoming else {
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

                let Ok(command) = serde_json::from_str::<ClientMessage>(&text) else {
                    continue;
                };
                if !command.is_valid() {
                    continue;
                }
                match command {
                    ClientMessage::Subscribe { target } => {
                        targets.insert(target);
                    }
                    ClientMessage::Unsubscribe { target } => {
                        targets.remove(&target);
                    }
                    ClientMessage::Ping => {
                        let Ok(encoded) = serde_json::to_string(&PublicServerMessage::Pong) else {
                            continue;
                        };
                        if socket.send(Message::Text(encoded.into())).await.is_err() {
                            break;
                        }
                    }
                }
            }
            event = events.recv() => {
                let envelope = match event {
                    Ok(PublicRealtimeEvent::Envelope(envelope)) => envelope,
                    Ok(PublicRealtimeEvent::Reset) => {
                        let _ = socket.send(Message::Close(None)).await;
                        break;
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        // The public protocol has no replay cursor. Closing is
                        // the recovery signal: the app reconnects, re-subscribes
                        // and invalidates caches loaded before the disconnect.
                        let _ = socket.send(Message::Close(None)).await;
                        break;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                };
                let messages = public_messages_from_frame(&envelope.payload);
                for mut message in messages {
                    attach_thread_project(&state, &mut message);
                    if !message.is_valid() {
                        continue;
                    }
                    if !targets.iter().any(|target| message.matches_target(target)) {
                        continue;
                    }
                    let Ok(encoded) = serde_json::to_string(&message) else {
                        continue;
                    };
                    if socket.send(Message::Text(encoded.into())).await.is_err() {
                        return;
                    }
                }
            }
        }
    }
}

fn attach_thread_project(state: &AppState, message: &mut PublicServerMessage) {
    let PublicServerMessage::Changed {
        entity: PublicEntity::Thread,
        id: Some(thread_id),
        metadata,
        ..
    } = message
    else {
        return;
    };
    if metadata
        .as_ref()
        .and_then(|metadata| metadata.project_id.as_ref())
        .is_some()
    {
        return;
    }
    let Ok(thread_id) = thread_id.parse::<loom_domain::ThreadId>() else {
        return;
    };
    let Some(thread) = state.registry.thread(&thread_id) else {
        return;
    };
    metadata
        .get_or_insert_with(ThreadChangeMetadata::default)
        .project_id = Some(thread.project_id.to_string());
}

async fn handle_worker(socket: WebSocket, state: AppState) {
    let (mut sink, mut stream) = socket.split();
    let (transport, mut outbound) = ChannelTransport::with_default_capacity();

    let pending_scope = Scope::Client(format!("worker-pending-{}", EventId::new()));
    let Ok(connection_id) = state.hub.connect(Box::new(transport), pending_scope).await else {
        return;
    };

    if send(
        &mut sink,
        &ServerMessage::Hello {
            protocol_version: crate::PROTOCOL_VERSION,
            // The agents this server can dispatch, so a worker can read each
            // one's catalogue at enrollment instead of waiting for a run.
            providers: state.providers().to_vec(),
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

    // A connection is a UI or a worker. A worker that enrolled is remembered
    // here so that closing the socket marks the host detached; a UI never sets
    // it and is unaffected by host bookkeeping.
    let mut enrolled_host: Option<HostId> = None;

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
            Ok(command) => handle_command(connection_id, command, &state, &mut enrolled_host).await,
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

    // A worker that dropped without saying goodbye is still gone: the socket
    // closing is what detaches the host. This is deliberately independent of
    // the server's own lifetime, so stopping a worker never touches the server.
    if let Some(host_id) = enrolled_host {
        // A terminal is driven through the worker that owns it, so a host with
        // no connection has no drivable sessions. The record survives — the
        // process may still be alive on that machine — but its status does not
        // claim to be usable.
        let disconnected_sessions: Vec<_> = state
            .terminals
            .list()
            .into_iter()
            .filter(|session| session.host_id == host_id)
            .collect();
        let changed = state
            .terminals
            .mark_host_disconnected(&host_id, loom_relay::now_ms());
        if changed > 0 {
            for session in &disconnected_sessions {
                crate::b9::publish_terminal_change(&state, session.thread_id.as_ref());
            }
            eprintln!(
                "loom-server: marked {changed} terminal session(s) disconnected with host {host_id}"
            );
        }
        if let Ok(events) = state
            .registry
            .mark_host_disconnected(&host_id, loom_relay::now_ms())
        {
            for event in &events {
                let _ = state.publish_domain_event(event);
            }
        }
    }

    let _ = state.hub.disconnect(connection_id).await;
    writer.abort();
}

async fn handle_command(
    connection_id: u64,
    command: ClientCommand,
    state: &AppState,
    enrolled_host: &mut Option<HostId>,
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
        ClientCommand::EnrollHost {
            host_id,
            name,
            data_dir,
            join_code,
        } => {
            let host_id = match join_code {
                Some(code) => match state.join_codes.consume(&code) {
                    Some(reserved) if host_id.as_ref().is_none_or(|id| id == &reserved) => {
                        Some(reserved)
                    }
                    Some(_) => {
                        return Some(ServerMessage::Error {
                            message: "join code is bound to a different host identity".into(),
                        })
                    }
                    None => {
                        return Some(ServerMessage::Error {
                            message: "join code is missing or expired".into(),
                        })
                    }
                },
                None => host_id,
            };
            match state.registry.enroll_host_with_data_dir(
                host_id,
                name,
                data_dir,
                loom_relay::now_ms(),
            ) {
                Ok((host, events)) => {
                    *enrolled_host = Some(host.id.clone());
                    // A host that just (re)connected is the authority on which
                    // of its terminal processes survived the gap. Reconcile in
                    // the background so enrollment does not wait on a relay
                    // round trip per session.
                    state.spawn_terminal_reconcile(host.id.clone());
                    Some(ServerMessage::HostEnrolled {
                        host,
                        event_id: publish_all(state, &events),
                    })
                }
                Err(error) => Some(ServerMessage::Error {
                    message: error.to_string(),
                }),
            }
        }
        ClientCommand::HostHeartbeat { host_id } => {
            if enrolled_host.as_ref() != Some(&host_id) {
                return Some(ServerMessage::Error {
                    message: "this connection is not enrolled as that host".into(),
                });
            }
            match state
                .registry
                .host_heartbeat(&host_id, loom_relay::now_ms())
            {
                Ok(host) => Some(ServerMessage::HostHeartbeatAck {
                    host_id,
                    last_seen_at_ms: host.last_seen_at_ms.unwrap_or(0),
                }),
                Err(error) => Some(ServerMessage::Error {
                    message: error.to_string(),
                }),
            }
        }
        ClientCommand::HostDisconnect { host_id } => {
            if enrolled_host.as_ref() != Some(&host_id) {
                return Some(ServerMessage::Error {
                    message: "this connection is not enrolled as that host".into(),
                });
            }
            let events = state
                .registry
                .mark_host_disconnected(&host_id, loom_relay::now_ms())
                .unwrap_or_default();
            publish_all(state, &events);
            state
                .terminals
                .mark_host_disconnected(&host_id, loom_relay::now_ms());
            // Cleared so the socket-close path does not mark it twice.
            *enrolled_host = None;
            Some(ServerMessage::HostDisconnected { host_id })
        }
        ClientCommand::RunReport { report } => {
            let report = *report;
            let Some(host_id) = enrolled_host.clone() else {
                return Some(ServerMessage::Error {
                    message: "run reports require an enrolled host".into(),
                });
            };
            if host_id != report.host_id {
                return Some(ServerMessage::Error {
                    message: "report names a different host than this connection enrolled as"
                        .into(),
                });
            }
            let run_id = report.event.run_id.clone();
            let outcome = state.apply_run_report(&host_id, report);
            let (accepted, detail) = match outcome {
                ReportOutcome::Applied => (true, None),
                ReportOutcome::Unknown => (
                    false,
                    Some("run is not in flight (already terminal)".into()),
                ),
                ReportOutcome::Mismatch(message) => (false, Some(message)),
                ReportOutcome::PublishFailed { error } => (
                    false,
                    Some(format!("relay could not publish the run event: {error}")),
                ),
            };
            Some(ServerMessage::RunReportAck {
                run_id,
                accepted,
                detail,
            })
        }
        ClientCommand::CatalogReport { report } => {
            // A catalogue describes the agent on the machine that reported it,
            // so the same ownership rule as every host-scoped frame applies:
            // one host cannot describe another's agent.
            let Some(host_id) = enrolled_host.clone() else {
                return Some(ServerMessage::Error {
                    message: "catalog reports require an enrolled host".into(),
                });
            };
            if host_id != report.host_id {
                return Some(ServerMessage::Error {
                    message: "report names a different host than this connection enrolled as"
                        .into(),
                });
            }
            state
                .catalogs
                .record(&host_id, &report.provider_id, report.catalog);
            None
        }
        ClientCommand::InteractionRequest { request } => {
            let request = *request;
            let Some(host_id) = enrolled_host.clone() else {
                return Some(ServerMessage::Error {
                    message: "interaction requests require an enrolled host".into(),
                });
            };
            if host_id != request.host_id {
                return Some(ServerMessage::Error {
                    message: "request names a different host than this connection enrolled as"
                        .into(),
                });
            }
            let request_id = request.request_id.clone();
            match state.record_interaction_request(request, loom_relay::now_ms()) {
                RecordOutcome::Recorded(interaction) => {
                    Some(ServerMessage::InteractionRequestAck {
                        request_id,
                        interaction_id: Some(interaction.id.to_string()),
                        accepted: true,
                        detail: None,
                    })
                }
                RecordOutcome::Unknown(detail) => {
                    eprintln!("loom-server: refused an interaction request: {detail}");
                    Some(ServerMessage::InteractionRequestAck {
                        request_id,
                        interaction_id: None,
                        accepted: false,
                        detail: Some(detail),
                    })
                }
            }
        }
        ClientCommand::EnvironmentReport { report } => {
            let Some(host_id) = enrolled_host.clone() else {
                return Some(ServerMessage::Error {
                    message: "environment reports require an enrolled host".into(),
                });
            };
            if host_id != report.host_id {
                return Some(ServerMessage::Error {
                    message: "report names a different host than this connection enrolled as"
                        .into(),
                });
            }
            let environment_id = report.environment_id.clone();
            let outcome = state.apply_environment_report(&host_id, report);
            let (accepted, detail) = match outcome {
                EnvironmentReportOutcome::Applied => (true, None),
                EnvironmentReportOutcome::Stale => (
                    false,
                    Some("environment is not awaiting provisioning".into()),
                ),
                EnvironmentReportOutcome::Unknown => {
                    (false, Some("environment is not known".into()))
                }
                EnvironmentReportOutcome::Mismatch(message) => (false, Some(message)),
            };
            Some(ServerMessage::EnvironmentReportAck {
                environment_id,
                accepted,
                detail,
            })
        }
        ClientCommand::HostFileReport { report } => {
            // Like every other host-scoped upload, the report must come from the
            // connection enrolled as that host: one machine must not answer a
            // read another machine was asked to perform.
            let Some(host_id) = enrolled_host.clone() else {
                return Some(ServerMessage::Error {
                    message: "host file reports require an enrolled host".into(),
                });
            };
            if host_id != report.host_id {
                return Some(ServerMessage::Error {
                    message: "report names a different host than this connection enrolled as"
                        .into(),
                });
            }
            // An answer nobody is waiting for is dropped, not an error: a
            // request whose client gave up, or a redelivered answer, is normal
            // and telling the worker about it would only make it retry.
            state.host_files.resolve(report);
            None
        }
        ClientCommand::ScriptReport { report } => {
            // A script run is reported by the machine that ran it, so the same
            // ownership rule as every other host-scoped frame applies: one
            // machine must not end another's run.
            let Some(host_id) = enrolled_host.clone() else {
                return Some(ServerMessage::Error {
                    message: "script reports require an enrolled host".into(),
                });
            };
            if host_id != report.host_id {
                return Some(ServerMessage::Error {
                    message: "report names a different host than this connection enrolled as"
                        .into(),
                });
            }
            // A run the server already settled — because the user paused the
            // automation, or because the host went quiet and the reaper failed
            // it — is not an error to report: the kill the user asked for and
            // the report that follows are racing, and losing that race is the
            // normal outcome.
            let _ = state.apply_script_run_report(&host_id, report);
            None
        }
        ClientCommand::HostRpcReport { report } => {
            let Some(host_id) = enrolled_host.clone() else {
                return Some(ServerMessage::Error {
                    message: "host RPC reports require an enrolled host".into(),
                });
            };
            if host_id != report.host_id {
                return Some(ServerMessage::Error {
                    message: "report names a different host than this connection enrolled as"
                        .into(),
                });
            }
            // Workspace answers are private to the HTTP request that minted
            // the correlation id. Late or duplicate answers are normal after
            // a request timeout and are deliberately ignored.
            state.host_rpc.resolve(report);
            None
        }
        ClientCommand::TerminalReport { report } => {
            let Some(host_id) = enrolled_host.clone() else {
                return Some(ServerMessage::Error {
                    message: "terminal reports require an enrolled host".into(),
                });
            };
            if host_id != report.host_id {
                return Some(ServerMessage::Error {
                    message: "report names a different host than this connection enrolled as"
                        .into(),
                });
            }
            // Terminal answers are private to the HTTP request that minted the
            // correlation id. A late or duplicate answer is normal after a
            // timeout and is deliberately ignored.
            state.terminal.resolve(report);
            None
        }
        ClientCommand::Replay {
            scope,
            since,
            limit,
        } => match replay_to_connection(connection_id, scope.clone(), since, limit, state).await {
            Ok((count, has_more)) => Some(ServerMessage::ReplayComplete {
                scope,
                count,
                has_more,
            }),
            Err(message) => Some(ServerMessage::Error { message }),
        },
    }
}

/// Queues retained frames for `scope` to one connection, oldest first.
///
/// The stored payload already *is* the client-facing frame, so it is forwarded
/// verbatim: a replayed frame and a live one are byte-identical, which is what
/// lets a client merge them by event id alone.
async fn replay_to_connection(
    connection_id: u64,
    scope: Scope,
    since: Option<EventId>,
    limit: Option<usize>,
    state: &AppState,
) -> Result<(usize, bool), String> {
    let limit = limit.unwrap_or(500).clamp(1, 10_000);
    let (events, has_more) = match since {
        // Resume: page forward so a consumer that repeats the call with the
        // returned last id cannot skip a frame. Returning the newest frames
        // here would drop everything between the cursor and the page, and the
        // advanced cursor would make the gap unrecoverable.
        Some(since) => {
            let page = state
                .relay
                .replay_page_after(&scope, Some(since), now_ms(), limit)
                .map_err(|error| error.to_string())?;
            (page.events, page.has_more)
        }
        // Fresh view: the newest frames in the window, which is what a client
        // opening a scope wants. No cursor exists to advance past a gap, so
        // nothing can be lost.
        None => {
            let events = state
                .relay
                .replay_scope(&scope, limit)
                .map_err(|error| error.to_string())?;
            (events, false)
        }
    };
    let count = events.len();
    for envelope in events {
        let _ = state.hub.send_to(connection_id, envelope.payload).await;
    }
    Ok((count, has_more))
}

/// Publishes domain events in order and returns the last event id.
///
/// An empty string means nothing was published — a worker reconnect that
/// changed no state. The caller acks the command either way; the frame is a
/// convenience, not the ack.
fn publish_all(state: &AppState, events: &[loom_domain::DomainEvent]) -> String {
    let mut last = String::new();
    for event in events {
        if let Ok(envelope) = state.publish_domain_event(event) {
            last = envelope.event_id.to_string();
        }
    }
    last
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
