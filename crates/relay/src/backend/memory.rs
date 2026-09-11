//! In-process backend.
//!
//! This is the default for a single self-hosted server, and the reference
//! implementation of [`RelayBackend`]. It keeps each shard's log in memory
//! behind its own lock, which means:
//!
//! * append, replay and trim for different shards never contend;
//! * the per-shard cap ([`MemoryBackend::new`]'s `max_len`) bounds memory the
//!   same way a stream `MAXLEN` does for the external backends.
//!
//! It is deliberately not durable. A server restart loses the in-memory log,
//! which is the same guarantee as "the process was down and the grace window
//! was empty". Deployments that must survive a server restart without
//! disconnecting daemons swap in the Redis/NATS backend instead; nothing above
//! this trait changes.

use std::sync::{Mutex, MutexGuard};

use crate::error::{RelayError, Result};
use crate::scope::{ShardId, SHARD_COUNT};

use super::{LogRecord, RelayBackend};

/// In-memory, append-ordered, sharded log.
pub struct MemoryBackend {
    shards: Vec<Mutex<Vec<LogRecord>>>,
    max_len: usize,
}

impl MemoryBackend {
    /// Creates a backend holding at most `max_len` records per shard
    /// (minimum 1).
    pub fn new(max_len: usize) -> Self {
        let mut shards = Vec::with_capacity(usize::from(SHARD_COUNT));
        for _ in 0..SHARD_COUNT {
            shards.push(Mutex::new(Vec::new()));
        }
        Self {
            shards,
            max_len: max_len.max(1),
        }
    }

    /// The configured per-shard cap.
    pub fn max_len(&self) -> usize {
        self.max_len
    }

    /// Total records across all shards.
    pub fn total_len(&self) -> usize {
        self.shards
            .iter()
            .map(|shard| shard.lock().unwrap_or_else(|p| p.into_inner()).len())
            .sum()
    }

    fn shard(&self, shard: ShardId) -> Result<MutexGuard<'_, Vec<LogRecord>>> {
        let index = usize::from(shard);
        if index >= self.shards.len() {
            return Err(RelayError::backend(format!(
                "shard {shard} out of range (0..{SHARD_COUNT})"
            )));
        }
        Ok(self.shards[index]
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()))
    }
}

impl RelayBackend for MemoryBackend {
    fn shard_count(&self) -> u8 {
        SHARD_COUNT
    }

    fn append(&self, shard: ShardId, record: LogRecord) -> Result<()> {
        let mut guard = self.shard(shard)?;
        guard.push(record);
        let overflow = guard.len().saturating_sub(self.max_len);
        if overflow > 0 {
            guard.drain(0..overflow);
        }
        Ok(())
    }

    fn read(&self, shard: ShardId, from_ms: u64, limit: usize) -> Result<Vec<LogRecord>> {
        let guard = self.shard(shard)?;
        Ok(guard
            .iter()
            .filter(|record| record.created_at_ms >= from_ms)
            .take(limit)
            .cloned()
            .collect())
    }

    fn trim(&self, shard: ShardId, before_ms: u64) -> Result<u64> {
        let mut guard = self.shard(shard)?;
        let before = guard.len();
        guard.retain(|record| record.created_at_ms >= before_ms);
        Ok((before - guard.len()) as u64)
    }

    fn len(&self, shard: ShardId) -> Result<usize> {
        Ok(self.shard(shard)?.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event_id::EventId;
    use crate::scope::Scope;
    use bytes::Bytes;

    fn record(created_at_ms: u64) -> LogRecord {
        LogRecord {
            event_id: EventId::new(),
            scope: Scope::Thread("thr_1".into()),
            payload: Bytes::from_static(b"{}"),
            created_at_ms,
            origin: "node-a".into(),
        }
    }

    #[test]
    fn append_and_read_in_order() {
        let backend = MemoryBackend::new(100);
        for ts in [10, 20, 30] {
            backend.append(0, record(ts)).unwrap();
        }
        let read = backend.read(0, 0, 100).unwrap();
        let stamps: Vec<u64> = read.iter().map(|r| r.created_at_ms).collect();
        assert_eq!(stamps, vec![10, 20, 30]);
    }

    #[test]
    fn read_filters_by_timestamp_and_limit() {
        let backend = MemoryBackend::new(100);
        for ts in [10, 20, 30, 40] {
            backend.append(1, record(ts)).unwrap();
        }
        let read = backend.read(1, 25, 100).unwrap();
        let stamps: Vec<u64> = read.iter().map(|r| r.created_at_ms).collect();
        assert_eq!(stamps, vec![30, 40]);

        let limited = backend.read(1, 0, 2).unwrap();
        assert_eq!(limited.len(), 2);
        assert_eq!(limited[0].created_at_ms, 10);
    }

    #[test]
    fn trim_removes_only_older_records() {
        let backend = MemoryBackend::new(100);
        for ts in [10, 20, 30, 40] {
            backend.append(2, record(ts)).unwrap();
        }
        let removed = backend.trim(2, 30).unwrap();
        assert_eq!(removed, 2);
        let remaining: Vec<u64> = backend
            .read(2, 0, 100)
            .unwrap()
            .iter()
            .map(|r| r.created_at_ms)
            .collect();
        assert_eq!(remaining, vec![30, 40]);
    }

    #[test]
    fn max_len_drops_the_oldest() {
        let backend = MemoryBackend::new(3);
        for ts in [10, 20, 30, 40, 50] {
            backend.append(3, record(ts)).unwrap();
        }
        assert_eq!(backend.len(3).unwrap(), 3);
        let stamps: Vec<u64> = backend
            .read(3, 0, 100)
            .unwrap()
            .iter()
            .map(|r| r.created_at_ms)
            .collect();
        assert_eq!(stamps, vec![30, 40, 50]);
    }

    #[test]
    fn shards_are_isolated() {
        let backend = MemoryBackend::new(100);
        backend.append(0, record(10)).unwrap();
        backend.append(1, record(20)).unwrap();
        assert_eq!(backend.len(0).unwrap(), 1);
        assert_eq!(backend.len(1).unwrap(), 1);
        assert_eq!(backend.total_len(), 2);
        backend.trim(0, u64::MAX).unwrap();
        assert_eq!(backend.total_len(), 1);
    }

    #[test]
    fn out_of_range_shard_is_an_error() {
        let backend = MemoryBackend::new(10);
        assert!(backend.append(SHARD_COUNT, record(1)).is_err());
        assert!(backend.read(SHARD_COUNT, 0, 10).is_err());
    }
}
