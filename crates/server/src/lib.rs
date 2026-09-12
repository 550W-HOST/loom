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
pub mod build_info;
pub mod domain_state;
pub mod environments;
pub mod http;
pub mod hub_actor;
pub mod persistence;
pub mod protocol;
pub mod pump;
pub mod runs;
pub mod state;
pub mod transport;
pub mod ui;
pub mod ws;

pub use artifacts::{
    ArtifactClient, ArtifactDownload, Artifacts, InstallVersion, DIGEST_HEADER,
    INSTALL_DAEMON_PATH, INSTALL_VERSION_PATH,
};
pub use build_info::{version_line, COMMIT, TARGET, VERSION};
pub use domain_state::{CommandError, DomainRegistry};
pub use environments::{EnvironmentReportOutcome, ProvisionOutcome};
pub use hub_actor::{HubCommand, HubHandle};
pub use persistence::{DomainSnapshot, SnapshotError, SNAPSHOT_FILE};
pub use protocol::{ClientCommand, ServerMessage};
pub use pump::{Pump, PumpConfig};
pub use runs::{
    DispatchOutcome, ReconcileSummary, ReportOutcome, RunRecord, RunRegistry, StopOutcome,
};
pub use state::{relay_scope, AppState, BuildStateError};
pub use transport::ChannelTransport;

/// Protocol version reported by `/api/v1/version`.
pub const PROTOCOL_VERSION: u32 = 1;
