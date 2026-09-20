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
//! disconnecting workers swap in a different backend instead; nothing above
//! this trait changes.

use std::sync::{Mutex, MutexGuard};

use crate::error::{RelayError, Result};
use crate::event_id::EventId;
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

    fn read_after(
        &self,
        shard: ShardId,
        after: Option<EventId>,
        limit: usize,
    ) -> Result<Vec<LogRecord>> {
        let guard = self.shard(shard)?;
        Ok(guard
            .iter()
            .filter(|record| match after {
                None => true,
                Some(cursor) => record.event_id > cursor,
            })
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
        let read = backend.read_after(0, None, 100).unwrap();
        let stamps: Vec<u64> = read.iter().map(|r| r.created_at_ms).collect();
        assert_eq!(stamps, vec![10, 20, 30]);
    }

    #[test]
    fn read_after_returns_only_newer_records() {
        let backend = MemoryBackend::new(100);
        let first = record(10);
        let second = record(20);
        let third = record(30);
        for item in [&first, &second, &third] {
            backend.append(1, item.clone()).unwrap();
        }

        let all = backend.read_after(1, None, 100).unwrap();
        assert_eq!(all.len(), 3);

        let after_first = backend.read_after(1, Some(first.event_id), 100).unwrap();
        assert_eq!(
            after_first.iter().map(|r| r.event_id).collect::<Vec<_>>(),
            vec![second.event_id, third.event_id]
        );

        let limited = backend.read_after(1, None, 2).unwrap();
        assert_eq!(limited.len(), 2);

        let none_left = backend.read_after(1, Some(third.event_id), 100).unwrap();
        assert!(none_left.is_empty());
    }

    /// A burst larger than the read limit, all in one millisecond, must still
    /// make progress: the cursor is exclusive in id space, not time space.
    #[test]
    fn read_after_progresses_through_a_same_millisecond_burst() {
        let backend = MemoryBackend::new(1_000);
        for _ in 0..32 {
            backend.append(1, record(50)).unwrap();
        }

        let mut cursor = None;
        let mut delivered = 0;
        loop {
            let batch = backend.read_after(1, cursor, 4).unwrap();
            if batch.is_empty() {
                break;
            }
            delivered += batch.len();
            cursor = Some(batch.last().unwrap().event_id);
        }
        assert_eq!(delivered, 32);
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
            .read_after(2, None, 100)
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
            .read_after(3, None, 100)
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
        assert!(backend.read_after(SHARD_COUNT, None, 10).is_err());
    }
}
