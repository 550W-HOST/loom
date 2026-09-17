//! The loom control plane.
//!
//! This crate is the seam between HTTP/WebSocket and the relay. It exists to
//! prove one property end to end: **a producer never touches a connection**.
//!
//! The path an event takes:
//!
//! ```text
//!   HTTP handler ──▶ Relay::publish(scope, frame)      (append to the log)
//!                          │
//!   fixed readers  ────────┘  one task per shard, constant count
//!                          │
//!                          ▼
//!                    Hub actor ──▶ subscriber sockets   (idempotent fan-out)
//! ```
//!
//! Every hop is a real one, not a simplification: the handler does not know
//! who is subscribed, and the hub does not know what a thread is. That is what
//! makes the deployment shapes in `docs/architecture.md` reachable — a second
//! node joins by attaching readers to the same log, not by changing handlers.

#![forbid(unsafe_code)]

pub mod artifacts;
pub mod automation_execution;
pub mod automations;
pub mod automations_contract;
pub mod b10;
pub mod b5;
pub mod b6;
pub mod b7;
pub mod b8;
pub mod b9;
pub mod build_info;
pub mod domain_state;
pub mod environments;
pub mod file_previews;
pub mod host_files;
pub mod host_rpc;
pub mod http;
pub mod hub_actor;
pub mod interactions;
pub mod join_codes;
pub mod persistence;
pub mod protocol;
pub mod pump;
pub mod queue;
pub mod runs;
pub mod settings;
pub mod state;
pub mod terminals;
pub mod transport;
pub mod ui;
pub mod ws;

pub use artifacts::{
    ArtifactClient, ArtifactDownload, Artifacts, InstallVersion, DIGEST_HEADER,
    INSTALL_DAEMON_PATH, INSTALL_VERSION_PATH,
};
pub use automation_execution::AutomationExecutionReport;
pub use automations::{AutomationState, AutomationsRegistry, AUTOMATIONS_VERSION};
pub use b5::{MAX_FILE_CONTENT_BYTES, MAX_HTML_PREVIEW_BYTES, THREAD_COUNT_ROOT_PARENT};
pub use b7::MAX_ATTACHMENT_BYTES;
pub use b9::{MAX_FILE_OPERATION_BYTES, MAX_WRITE_BYTES};
pub use build_info::{version_line, COMMIT, TARGET, VERSION};
pub use domain_state::{CommandError, DomainRegistry};
pub use environments::{EnvironmentReportOutcome, ProvisionOutcome};
pub use host_files::{HostFileBroker, HostFileTransportError, HOST_FILE_TIMEOUT};
pub use host_rpc::{HostRpcBroker, HostRpcTransportError, HOST_RPC_TIMEOUT};
pub use hub_actor::{HubCommand, HubHandle};
pub use interactions::DeliverOutcome;
pub use persistence::{DomainSnapshot, SnapshotError, SNAPSHOT_FILE};
pub use protocol::{
    ClientMessage, DaemonClientMessage, DaemonServerMessage, ServerMessage, SubscriptionTarget,
};
pub use pump::{Pump, PumpConfig};
pub use queue::{DeliveryOutcome, WaitingOn};
pub use runs::{
    DispatchOutcome, ReconcileSummary, ReportOutcome, RunRecord, RunRegistry, StopOutcome,
};
pub use state::{relay_scope, AppState, BuildStateError};
pub use terminals::{TerminalBroker, TerminalSessions, TerminalTransportError, TERMINAL_TIMEOUT};
pub use transport::ChannelTransport;

/// Protocol version reported by `/api/v1/version` and negotiated by daemons.
///
/// Version 3 separates the public bb `/ws` protocol from the daemon
/// `/internal/ws` protocol and introduces the daemon `hello` handshake. Server
/// and daemon must upgrade together (or use the existing daemon self-update
/// path).
pub const PROTOCOL_VERSION: u32 = 3;

/// Explicit transport negotiation token for the public bb realtime protocol.
///
/// Connections without this token are handled only by the temporary v2 daemon
/// migration shim; client role is never inferred from `Origin`.
pub const PUBLIC_WS_SUBPROTOCOL: &str = "loom-bb-realtime-v1";
