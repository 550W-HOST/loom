//! The client wire protocol.
//!
//! Kept in its own module for one reason: the frame a client receives must be
//! byte-identical whether it arrived live or from replay. That is only possible
//! if the frame is built in exactly one place, before it is stored.
//!
//! Consequently **the payload stored in the relay log is the client-facing
//! frame**, not a bare domain payload wrapped later. Two things fall out of
//! that, and both are properties this design wants:
//!
//! * a reconnecting client merges backlog and live frames with no shape
//!   translation — it only deduplicates by event id;
//! * the hub stays protocol-agnostic: it forwards opaque bytes and never needs
//!   to know that an event has an id at all.
//!
//! The cost is that the event id appears both in the relay envelope and inside
//! the frame. That duplication is deliberate: the envelope needs the id for
//! ordering, trimming and dedup, while the frame needs it as the client's
//! resume cursor.

use bytes::Bytes;
use loom_domain::{EnvironmentId, Host, RunId};
use loom_provider_protocol::{EnvironmentProvisionReport, HostRpcReport, ProviderReport};
use loom_relay::envelope::Envelope;
use loom_relay::event_id::EventId;
use loom_relay::scope::Scope;
use serde::{Deserialize, Serialize};

/// Messages a client may send.
///
/// The same socket serves UI clients and daemons. A UI subscribes to the rooms
/// it displays; a daemon enrolls, then follows its own `host:{id}` room. Both
/// connect outbound to the same URL, which is what keeps a daemon independent
/// of the server's process tree.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientCommand {
    /// Start receiving frames for a scope.
    Subscribe {
        /// The scope to follow.
        scope: Scope,
    },
    /// Stop receiving frames for a scope.
    Unsubscribe {
        /// The scope to leave.
        scope: Scope,
    },
    /// Liveness probe.
    Ping,
    /// A daemon announces itself as a host.
    ///
    /// `host_id` is the identity the daemon was previously given, so a
    /// reconnect is a status change rather than a new machine. Omitted on a
    /// first enrollment, where the server mints one and returns it in
    /// [`ServerMessage::HostEnrolled`].
    EnrollHost {
        /// The daemon's existing identity, if it has one.
        #[serde(default)]
        host_id: Option<loom_domain::HostId>,
        /// The machine's display name.
        name: String,
        /// The daemon's own data directory on this machine.
        ///
        /// Thread storage is layout the **daemon** owns
        /// (`<data_dir>/thread-storage/<thread_id>`), so the control plane can
        /// only name it when the machine says where its data lives. Additive
        /// on the wire: an older daemon sends nothing and a storage route
        /// answers `501` rather than guessing a path.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        data_dir: Option<String>,
        /// A one-time code issued by `hosts.createJoinCode`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        join_code: Option<String>,
    },
    /// A daemon reports that it is still alive.
    HostHeartbeat {
        /// The host the daemon was enrolled as.
        host_id: loom_domain::HostId,
    },
    /// A daemon leaves deliberately, before the socket closes.
    HostDisconnect {
        /// The host the daemon was enrolled as.
        host_id: loom_domain::HostId,
    },
    /// A daemon reports what a provider did during an in-flight run.
    ///
    /// This is the execution plane's upload path: the server turns the report
    /// into a `thread_run_event` and publishes it to the thread scope through
    /// the relay, so it is replayable like any other event. The socket only
    /// carries the observation; it never carries the resulting fan-out.
    ///
    /// The report is boxed: a contract run event is much larger than the
    /// transport commands around it, and an unboxed variant would widen every
    /// `ClientCommand`.
    RunReport {
        /// The run observation.
        report: Box<ProviderReport>,
    },
    /// A daemon reports that a provider is blocked on a permission request.
    ///
    /// This is a *request*, not an observation: the turn cannot proceed until
    /// the control plane has recorded the question and a client has answered
    /// it. The server records a durable interaction, publishes it to the
    /// thread scope, and acknowledges with the interaction id. The answer
    /// travels back down through the relay (see
    /// [`loom_provider_protocol::InteractionResolutionFrame`]), never as a
    /// reply to this frame, because the client that answers may not be the
    /// daemon's peer.
    InteractionRequest {
        /// The permission request.
        request: Box<loom_provider_protocol::InteractionRequest>,
    },
    /// A daemon reports the outcome of provisioning a managed environment's
    /// workspace.
    ///
    /// The server turns it into `environment_status_changed` (and, on success,
    /// records the workspace path) and publishes it to the project scope.
    EnvironmentReport {
        /// The provisioning observation.
        report: EnvironmentProvisionReport,
    },
    /// A daemon answers one [`loom_provider_protocol::HostFileRequest`].
    ///
    /// The request itself arrives through the relay (see
    /// [`crate::protocol`] and `docs/contract.md`); the answer comes back up
    /// this socket because it satisfies exactly one waiting HTTP request and
    /// must not be fanned out to every client watching the host's room.
    HostFileReport {
        /// The read or listing result, tagged with the request it answers.
        report: loom_provider_protocol::HostFileReport,
    },
    /// A daemon answers one host workspace RPC.
    ///
    /// The report is consumed by the server-side broker and is not fanned out
    /// to UI clients: it satisfies the one HTTP request that owns its
    /// correlation id.
    HostRpcReport {
        /// The workspace operation result.
        report: HostRpcReport,
    },
    /// A daemon answers one terminal request.
    ///
    /// Like a host file or workspace answer, this satisfies exactly one
    /// waiting HTTP request and is consumed by the broker, never fanned out.
    TerminalReport {
        /// The terminal operation result.
        report: loom_provider_protocol::TerminalReport,
    },
    /// Ask the server to replay retained frames for a scope to this
    /// connection.
    ///
    /// **Subscribe first, then replay.** Live frames that arrive in between
    /// are queued on this connection and are also present in the replay window,
    /// so the caller drops the duplicate by `event_id`. This is what makes a
    /// reconnecting daemon recover a dispatch it missed while disconnected.
    Replay {
        /// The scope to read back.
        scope: Scope,
        /// Page forward from this event id, exclusively. Omit for the newest
        /// frames in the retention window.
        ///
        /// The two cases differ deliberately. With a cursor the server returns
        /// the **oldest** frames after it, so a consumer that repeats the call
        /// with the returned last id always advances and always converges.
        /// Without one it returns the **newest**, which is what a client
        /// opening a scope wants. See [`ServerMessage::ReplayComplete`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        since: Option<EventId>,
        /// Maximum frames per page. The server caps it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<usize>,
    },
}

/// Messages the server sends.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    /// Sent once per connection.
    Welcome {
        /// The connection's id within this node.
        connection_id: u64,
        /// The relay protocol version.
        protocol_version: u32,
    },
    /// Acknowledges a subscription.
    Subscribed {
        /// Echoes the scope.
        scope: Scope,
        /// Whether this connection was the room's first subscriber.
        first_subscriber: bool,
    },
    /// Acknowledges an unsubscription.
    Unsubscribed {
        /// Echoes the scope.
        scope: Scope,
    },
    /// One relayed event.
    Event {
        /// The event's identity, also the resume cursor.
        event_id: String,
        /// The scope it belongs to.
        scope: Scope,
        /// The domain payload, as text.
        payload: String,
        /// Producer wall-clock time in milliseconds.
        created_at_ms: u64,
    },
    /// Reply to [`ClientCommand::Ping`].
    Pong,
    /// A rejected command.
    Error {
        /// Why it was rejected.
        message: String,
    },
    /// Acknowledges [`ClientCommand::EnrollHost`].
    HostEnrolled {
        /// The host, with the identity to reuse on reconnect.
        host: Host,
        /// The id of the `host_registered` or `host_status_changed` event, or
        /// an empty string when the reconnect changed nothing.
        event_id: String,
    },
    /// Acknowledges [`ClientCommand::HostHeartbeat`].
    HostHeartbeatAck {
        /// The host that was kept alive.
        host_id: loom_domain::HostId,
        /// Its new last-seen wall-clock time.
        last_seen_at_ms: u64,
    },
    /// Acknowledges [`ClientCommand::HostDisconnect`].
    HostDisconnected {
        /// The host that was marked detached.
        host_id: loom_domain::HostId,
    },
    /// Acknowledges [`ClientCommand::RunReport`].
    RunReportAck {
        /// The run the report was about.
        run_id: RunId,
        /// Whether the report was applied. `false` means the run was already
        /// terminal or the report contradicted the dispatcher's record; both
        /// are normal under redelivery.
        accepted: bool,
        /// Why it was not applied, when it was not.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    /// Acknowledges [`ClientCommand::InteractionRequest`].
    InteractionRequestAck {
        /// The daemon's identity for the request, echoed so it can match the
        /// answer that will arrive through the relay.
        request_id: String,
        /// loom's durable interaction id, once the question is recorded. Absent
        /// when the request was refused, in which case `detail` says why.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        interaction_id: Option<String>,
        /// Whether the question was recorded. `false` means the request named
        /// an unknown or foreign run; the daemon still holds the request open
        /// and will settle it through its own outcome, never by assuming a yes.
        accepted: bool,
        /// Why it was not accepted, when it was not.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    /// Acknowledges [`ClientCommand::EnvironmentReport`].
    EnvironmentReportAck {
        /// The environment the report was about.
        environment_id: EnvironmentId,
        /// Whether the report was applied. `false` means the environment was
        /// not awaiting provisioning (a duplicate or a late redelivery).
        accepted: bool,
        /// Why it was not applied, when it was not.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    /// Acknowledges [`ClientCommand::Replay`] after the backlog was queued.
    ReplayComplete {
        /// The scope that was replayed.
        scope: Scope,
        /// How many frames were queued.
        count: usize,
        /// Whether more frames follow this page. A consumer resuming from a
        /// cursor must keep paging while this is `true`; stopping early leaves
        /// a gap it can no longer recover, because it already advanced its
        /// cursor past it.
        has_more: bool,
    },
}

/// Builds the frame a client receives for an event.
///
/// Called once, before the event is stored, so live and replayed frames cannot
/// diverge.
pub fn build_event_frame(
    scope: &Scope,
    payload: &[u8],
    event_id: EventId,
    created_at_ms: u64,
) -> Bytes {
    let frame = ServerMessage::Event {
        event_id: event_id.to_string(),
        scope: scope.clone(),
        payload: String::from_utf8_lossy(payload).into_owned(),
        created_at_ms,
    };
    // Serializing a struct of strings, a scope and integers cannot fail.
    Bytes::from(serde_json::to_vec(&frame).expect("a ServerMessage always serializes to JSON"))
}

/// Converts a stored envelope into the frame clients receive.
///
/// Only valid for envelopes whose payload was produced by
/// [`build_event_frame`]; used by replay, which must return the stored bytes
/// untouched.
pub fn frame_from_envelope(envelope: &Envelope) -> Bytes {
    envelope.payload.clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_host_enrollment_with_and_without_an_identity() {
        let fresh: ClientCommand =
            serde_json::from_str(r#"{"type":"enroll_host","name":"laptop"}"#).unwrap();
        assert_eq!(
            fresh,
            ClientCommand::EnrollHost {
                host_id: None,
                name: "laptop".into(),
                data_dir: None,
                join_code: None,
            }
        );

        let host_id = loom_domain::HostId::mint();
        let returning: ClientCommand = serde_json::from_str(&format!(
            r#"{{"type":"enroll_host","host_id":"{host_id}","name":"laptop"}}"#
        ))
        .unwrap();
        assert_eq!(
            returning,
            ClientCommand::EnrollHost {
                host_id: Some(host_id),
                name: "laptop".into(),
                data_dir: None,
                join_code: None,
            }
        );

        // The data directory is additive on the wire: an older daemon omits it
        // and the server records nothing, rather than failing to enroll.
        let reporting: ClientCommand = serde_json::from_str(
            r#"{"type":"enroll_host","name":"laptop","data_dir":"/var/lib/loom"}"#,
        )
        .unwrap();
        assert_eq!(
            reporting,
            ClientCommand::EnrollHost {
                host_id: None,
                name: "laptop".into(),
                data_dir: Some("/var/lib/loom".into()),
                join_code: None,
            }
        );
    }

    #[test]
    fn host_replies_round_trip() {
        let (host, _) = loom_domain::Host::register("laptop", 1).unwrap();
        let enrolled = ServerMessage::HostEnrolled {
            host: host.clone(),
            event_id: "01M".into(),
        };
        let encoded = serde_json::to_string(&enrolled).unwrap();
        assert_eq!(
            serde_json::from_str::<ServerMessage>(&encoded).unwrap(),
            enrolled
        );

        let ack = ServerMessage::HostHeartbeatAck {
            host_id: host.id.clone(),
            last_seen_at_ms: 9,
        };
        assert_eq!(
            serde_json::from_str::<ServerMessage>(&serde_json::to_string(&ack).unwrap()).unwrap(),
            ack
        );
    }

    #[test]
    fn parses_subscribe_with_a_tagged_scope() {
        let command: ClientCommand =
            serde_json::from_str(r#"{"type":"subscribe","scope":{"kind":"thread","id":"thr_1"}}"#)
                .unwrap();
        assert_eq!(
            command,
            ClientCommand::Subscribe {
                scope: Scope::Thread("thr_1".into())
            }
        );
    }

    #[test]
    fn parses_ping_and_rejects_unknown_commands() {
        assert_eq!(
            serde_json::from_str::<ClientCommand>(r#"{"type":"ping"}"#).unwrap(),
            ClientCommand::Ping
        );
        assert!(serde_json::from_str::<ClientCommand>(r#"{"type":"nope"}"#).is_err());
    }

    #[test]
    fn a_built_frame_round_trips_as_a_server_message() {
        let scope = Scope::Thread("thr_1".into());
        let event_id = EventId::new();
        let frame = build_event_frame(&scope, b"{\"n\":1}", event_id, 7);

        let decoded: ServerMessage = serde_json::from_slice(&frame).unwrap();
        assert_eq!(
            decoded,
            ServerMessage::Event {
                event_id: event_id.to_string(),
                scope,
                payload: "{\"n\":1}".into(),
                created_at_ms: 7,
            }
        );
    }

    #[test]
    fn the_frame_carries_the_resume_cursor_and_scope() {
        let scope = Scope::Host("host_1".into());
        let event_id = EventId::new();
        let frame = build_event_frame(&scope, b"{}", event_id, 1);
        let value: serde_json::Value = serde_json::from_slice(&frame).unwrap();

        assert_eq!(value["type"], "event");
        assert_eq!(value["event_id"], event_id.to_string());
        assert_eq!(value["scope"]["kind"], "host");
        assert_eq!(value["scope"]["id"], "host_1");
    }

    #[test]
    fn welcome_and_error_serialize_with_a_type_tag() {
        let welcome = serde_json::to_value(ServerMessage::Welcome {
            connection_id: 3,
            protocol_version: crate::PROTOCOL_VERSION,
        })
        .unwrap();
        assert_eq!(welcome["type"], "welcome");
        assert_eq!(welcome["connection_id"], 3);

        let error = serde_json::to_value(ServerMessage::Error {
            message: "nope".into(),
        })
        .unwrap();
        assert_eq!(error["type"], "error");
    }
}
