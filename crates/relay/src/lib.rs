//! The loom relay layer.
//!
//! The control plane never touches a connection. It only publishes an
//! [`Envelope`] to a [`Scope`]. The relay decides where that envelope is
//! stored and how it is made available to every node, and the connection layer
//! ([`loom_relay_hub`]) decides which sockets on this node receive it.
//!
//! Three properties make this layer worth its own crate:
//!
//! 1. **Fixed fan-in.** Events route into a constant number of shards
//!    ([`SHARD_COUNT`]), so the number of blocked readers is
//!    `node_count * SHARD_COUNT` and never grows with the number of live
//!    threads, projects, or hosts.
//! 2. **Replay.** Every envelope carries a monotonic [`EventId`], and the log
//!    retains a bounded grace window. A node that was down, or a client that
//!    reconnected, replays exactly what it missed instead of re-fetching the
//!    world.
//! 3. **Idempotence.** Because replay exists, delivery can happen twice, so
//!    every consumer deduplicates by [`EventId`] ([`dedup::SeenSet`]).
//!
//! The storage behind this is pluggable via [`RelayBackend`]. The default is an
//! in-process [`backend::memory::MemoryBackend`], which needs no external
//! service and is enough for a single self-hosted server. [`backend::disk::DiskBackend`]
//! adds a dependency-free local log so a restart replays the grace window
//! instead of losing it. Redis/NATS backends exist for a log shared across
//! nodes; none of them change anything above this line.

#![forbid(unsafe_code)]

pub mod backend;
pub mod dedup;
pub mod envelope;
pub mod error;
pub mod event_id;
pub mod relay;
pub mod retention;
pub mod scope;

pub use backend::{LogRecord, RelayBackend, SharedBackend};
pub use envelope::{Envelope, NodeId, WireEnvelope};
pub use error::{RelayError, Result};
pub use event_id::EventId;
pub use relay::{MaintenanceReport, Relay};
pub use retention::Retention;
pub use scope::{Scope, ShardId, SHARD_COUNT};

use std::time::{SystemTime, UNIX_EPOCH};

/// Wall-clock milliseconds since the Unix epoch.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
