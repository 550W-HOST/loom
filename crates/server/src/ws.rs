//! WebSocket surface for clients and daemons.
//!
//! The client protocol is intentionally tiny and scoped. A UI says:
//!
//! ```json
//! // client -> server
//! {"type":"subscribe","scope":{"kind":"thread","id":"thr_1"}}
//! {"type":"unsubscribe","scope":{"kind":"thread","id":"thr_1"}}
//! {"type":"ping"}
//! ```
//!
//! A daemon uses the same socket to enroll, then follows its own `host:{id}`
//! room:
//!
//! ```json
//! // daemon -> server
//! {"type":"enroll_host","name":"laptop"}
//! {"type":"host_heartbeat","host_id":"host_..."}
//! {"type":"host_disconnect","host_id":"host_..."}
//!
//! // server -> daemon
//! {"type":"welcome","connection_id":1,"protocol_version":2}
//! {"type":"host_enrolled","host":{...},"event_id":"01M..."}
//! {"type":"host_heartbeat_ack","host_id":"host_...","last_seen_at_ms":1}
//! {"type":"host_disconnected","host_id":"host_..."}
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
use loom_domain::HostId;
use loom_relay::event_id::EventId;
use loom_relay::now_ms;
use loom_relay::scope::Scope;

use crate::environments::EnvironmentReportOutcome;
use crate::interactions::RecordOutcome;
use crate::protocol::{ClientCommand, ServerMessage};
use crate::runs::ReportOutcome;
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

    // A connection is a UI or a daemon. A daemon that enrolled is remembered
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

    // A daemon that dropped without saying goodbye is still gone: the socket
    // closing is what detaches the host. This is deliberately independent of
    // the server's own lifetime, so stopping a daemon never touches the server.
    if let Some(host_id) = enrolled_host {
        // A terminal is driven through the daemon that owns it, so a host with
        // no connection has no drivable sessions. The record survives — the
        // process may still be alive on that machine — but its status does not
        // claim to be usable.
        let changed = state
            .terminals
            .mark_host_disconnected(&host_id, loom_relay::now_ms());
        if changed > 0 {
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
                RecordOutcome::Unknown(detail) => Some(ServerMessage::InteractionRequestAck {
                    request_id,
                    interaction_id: None,
                    accepted: false,
                    detail: Some(detail),
                }),
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
            // and telling the daemon about it would only make it retry.
            state.host_files.resolve(report);
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
/// An empty string means nothing was published — a daemon reconnect that
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
