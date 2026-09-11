//! Storage behind the relay.
//!
//! A backend stores append-ordered records per shard and can replay and trim
//! them. Everything above this trait — control plane, connection layer, the
//! frames themselves — is independent of which backend is in use. That is what
//! makes "single-process in-memory", "durable on local disk" and "shared
//! Redis Streams" a deployment choice rather than a rewrite.
//!
//! Two backends ship here: [`memory::MemoryBackend`] (the zero-configuration
//! default) and [`disk::DiskBackend`], which keeps the replay window across a
//! process restart in one crash-safe append-only file per shard.
//!
//! A third, [`redis::RedisBackend`], moves the log into Redis Streams so that
//! a restart is transparent to connected daemons *and* a second node can
//! attach to the same window. It is optional configuration, not a dependency:
//! the default build still needs no external service.

pub mod disk;
pub mod memory;
pub mod redis;

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

    /// Reads records strictly newer than `after`, in append order, up to
    /// `limit` records. `None` starts from the oldest retained record.
    ///
    /// # Why the cursor is a parameter and not a caller-side filter
    ///
    /// A reader resumes from the last [`EventId`] it delivered, so what it
    /// needs is "the next `limit` records *after* this one". Filtering that in
    /// the caller is **not** equivalent. A timestamp-bounded read that applies
    /// `limit` before the cursor filter can return a full batch that the caller
    /// must discard entirely, and then return that same batch forever. That is
    /// not hypothetical: a burst of more than `limit` events minted in the same
    /// millisecond stalls that shard's reader permanently, silently dropping
    /// every later event behind it.
    ///
    /// Expressing the cursor in the read makes progress unconditional — either
    /// the backend returns records newer than the cursor, or it returns nothing
    /// because there are none.
    fn read_after(
        &self,
        shard: ShardId,
        after: Option<EventId>,
        limit: usize,
    ) -> Result<Vec<LogRecord>>;

    /// Drops records with `created_at_ms < before_ms`, returning how many were
    /// removed.
    fn trim(&self, shard: ShardId, before_ms: u64) -> Result<u64>;

    /// Number of records currently held by a shard.
    fn len(&self, shard: ShardId) -> Result<usize>;

    /// Whether a shard holds no records.
    fn is_empty(&self, shard: ShardId) -> Result<bool> {
        self.len(shard).map(|len| len == 0)
    }

    /// A latched backend-level failure, if any.
    ///
    /// A backend whose IO is synchronous reports failures at the call site and
    /// returns `None` here. A backend that hands work to a writer and returns
    /// before the write happens cannot: the failure surfaces after the call it
    /// belongs to, so it is latched and reported here instead. Reads keep
    /// working in that state (the in-memory view is intact), which is exactly
    /// why this must be observable — otherwise a durability failure looks like
    /// a healthy server until the next restart loses the window.
    fn backend_error(&self) -> Option<String> {
        None
    }
}

/// A shared, type-erased backend handle.
pub type SharedBackend = Arc<dyn RelayBackend>;
