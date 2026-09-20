//! The relay's storage, backed by the store.
//!
//! This is the `RelayBackend` the server runs on: the same frames the file
//! backend kept in `shard-*.log`, kept in the store's `relay_event` table. It
//! lives here rather than in the relay crate because the relay must not depend
//! on the store — the store is the server's, and the backend trait is the relay's
//! — so the server is where the two meet.
//!
//! **Writes are synchronous.** `append` commits the frame before it returns, and
//! `flush` has nothing left to do. That is the honest shape for a backend whose
//! caller is the publish path: the frame is either in the database or the
//! publish failed, and a crash cannot leave a frame that a reader acknowledged
//! and a store never heard of. It is the same thing the file backend did — it
//! wrote the record and synced on flush — with the database's own commit as the
//! boundary instead of a file's.

use std::sync::{Arc, Mutex};

use loom_relay::backend::{LogRecord, RelayBackend};
use loom_relay::error::Result;
use loom_relay::{EventId, ShardId, SHARD_COUNT};

use super::Store;

/// A relay over the store's `relay_event` table.
#[derive(Debug)]
pub struct StoreBackend {
    store: Arc<Mutex<Store>>,
    /// The per-shard cap, matching the file backends' `backend_max_len`.
    max_len: usize,
}

impl StoreBackend {
    /// A backend over `store`, keeping at most `max_len` frames per shard.
    pub fn new(store: Arc<Mutex<Store>>, max_len: usize) -> Self {
        Self {
            store,
            max_len: max_len.max(1),
        }
    }

    /// The store this backend writes to.
    pub fn store(&self) -> Arc<Mutex<Store>> {
        Arc::clone(&self.store)
    }

    /// The store, locked for one operation.
    fn locked(&self) -> std::sync::MutexGuard<'_, Store> {
        self.store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl RelayBackend for StoreBackend {
    fn shard_count(&self) -> u8 {
        SHARD_COUNT
    }

    fn append(&self, shard: ShardId, record: LogRecord) -> Result<()> {
        self.locked()
            .append_relay_event(shard, &record, self.max_len)
            .map_err(|error| loom_relay::error::RelayError::backend(error.to_string()))
    }

    fn read_after(
        &self,
        shard: ShardId,
        after: Option<EventId>,
        limit: usize,
    ) -> Result<Vec<LogRecord>> {
        self.locked()
            .read_relay_events(shard, after, limit)
            .map_err(|error| loom_relay::error::RelayError::backend(error.to_string()))
    }

    fn trim(&self, shard: ShardId, before_ms: u64) -> Result<u64> {
        self.locked()
            .trim_relay_events(shard, before_ms)
            .map_err(|error| loom_relay::error::RelayError::backend(error.to_string()))
    }

    fn len(&self, shard: ShardId) -> Result<usize> {
        self.locked()
            .relay_event_count(shard)
            .map_err(|error| loom_relay::error::RelayError::backend(error.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_backend_over_the_store_answers_with_its_shard_count() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
        let backend = StoreBackend::new(store, 100);
        assert_eq!(backend.shard_count(), SHARD_COUNT);
        assert_eq!(backend.len(0).unwrap(), 0);
    }
}
