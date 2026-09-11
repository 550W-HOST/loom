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

pub mod http;
pub mod hub_actor;
pub mod protocol;
pub mod pump;
pub mod state;
pub mod transport;
pub mod ws;

pub use hub_actor::{HubCommand, HubHandle};
pub use protocol::{ClientCommand, ServerMessage};
pub use pump::{Pump, PumpConfig};
pub use state::{AppState, BuildStateError};
pub use transport::ChannelTransport;

/// Protocol version reported by `/api/v1/version`.
pub const PROTOCOL_VERSION: u32 = 1;
