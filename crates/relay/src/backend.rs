//! Storage behind the relay.
//!
//! A backend stores append-ordered records per shard and can replay and trim
//! them. Everything above this trait — control plane, connection layer, the
//! frames themselves — is independent of which backend is in use. That is what
//! makes "single-process in-memory", "durable on local disk" and "shared
//! Redis/NATS" a deployment choice rather than a rewrite.
//!
//! Two backends ship here: [`memory::MemoryBackend`] (the zero-configuration
//! default) and [`disk::DiskBackend`], which keeps the replay window across a
//! process restart in one crash-safe append-only file per shard.

pub mod disk;
pub mod memory;

use std::sync::Arc;

use bytes::Bytes;

use crate::error::Result;
use crate::event_id::EventId;
use crate::scope::{Scope, ShardId};

/// One record as stored by a backend.
///
/// This is [`crate::Envelope`] minus anything delivery-specific: `exclude`
/// only matters on the path from producer to node, and does not belong in
/// storage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogRecord {
    /// Monotonic identity of this record.
    pub event_id: EventId,
    /// The room it belongs to.
    pub scope: Scope,
    /// The frame.
    pub payload: Bytes,
    /// Producer wall-clock time in milliseconds.
    pub created_at_ms: u64,
    /// Producing node.
    pub origin: String,
}

/// Append-ordered storage for relayed events.
///
/// Implementations must be `Send + Sync`: the relay is shared across all
/// server tasks.
pub trait RelayBackend: Send + Sync {
    /// Number of shards this backend stores. Must equal
    /// [`crate::SHARD_COUNT`]; the relay asserts this at construction.
    fn shard_count(&self) -> u8;

    /// Appends a record to a shard.
    fn append(&self, shard: ShardId, record: LogRecord) -> Result<()>;

    /// Reads records with `created_at_ms >= from_ms`, in append order, up to
    /// `limit` records.
    fn read(&self, shard: ShardId, from_ms: u64, limit: usize) -> Result<Vec<LogRecord>>;

    /// Drops records with `created_at_ms < before_ms`, returning how many were
    /// removed.
    fn trim(&self, shard: ShardId, before_ms: u64) -> Result<u64>;

    /// Number of records currently held by a shard.
    fn len(&self, shard: ShardId) -> Result<usize>;

    /// Whether a shard holds no records.
    fn is_empty(&self, shard: ShardId) -> Result<bool> {
        self.len(shard).map(|len| len == 0)
    }
}

/// A shared, type-erased backend handle.
pub type SharedBackend = Arc<dyn RelayBackend>;
