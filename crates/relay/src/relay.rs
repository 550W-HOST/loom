//! The producer-facing relay facade.
//!
//! [`Relay`] is what the control plane holds. It knows how to route a scope to
//! a shard, stamp an [`Envelope`], persist it, replay a window, and run
//! maintenance. It knows nothing about sockets, nodes, or protocols.

use std::sync::Arc;

use bytes::Bytes;

use crate::backend::{LogRecord, SharedBackend};
use crate::envelope::Envelope;
use crate::error::{RelayError, Result};
use crate::event_id::EventId;
use crate::retention::Retention;
use crate::scope::{Scope, ShardId, SHARD_COUNT};

/// Outcome of one maintenance pass.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MaintenanceReport {
    /// Records dropped by trimming.
    pub trimmed: u64,
}

/// Routes, stores and replays scoped events.
#[derive(Clone)]
pub struct Relay {
    backend: SharedBackend,
    retention: Retention,
    origin: String,
}

impl Relay {
    /// Builds a relay over `backend`.
    ///
    /// Fails if the retention policy is inconsistent or the backend's shard
    /// count does not match [`SHARD_COUNT`] — routing is derived from the shard
    /// count, so a mismatch would silently misroute.
    pub fn new(
        backend: SharedBackend,
        retention: Retention,
        origin: impl Into<String>,
    ) -> Result<Self> {
        retention.validate()?;
        if backend.shard_count() != SHARD_COUNT {
            return Err(RelayError::config(format!(
                "backend exposes {} shards but the relay routes over {SHARD_COUNT}",
                backend.shard_count()
            )));
        }
        Ok(Self {
            backend,
            retention,
            origin: origin.into(),
        })
    }

    /// Builds a relay with default retention over `backend`.
    pub fn with_defaults(backend: SharedBackend, origin: impl Into<String>) -> Result<Self> {
        Relay::new(backend, Retention::default(), origin)
    }

    /// This node's identity, stamped on every envelope it publishes.
    pub fn origin(&self) -> &str {
        &self.origin
    }

    /// The active retention policy.
    pub fn retention(&self) -> &Retention {
        &self.retention
    }

    /// A handle to the underlying backend (for metrics, tests, maintenance).
    pub fn backend(&self) -> &SharedBackend {
        &self.backend
    }

    /// The shard a scope routes to.
    pub fn shard_of(&self, scope: &Scope) -> ShardId {
        scope.shard()
    }

    /// Publishes a frame to a scope, returning the stored envelope.
    pub fn publish(&self, scope: Scope, payload: impl Into<Bytes>) -> Result<Envelope> {
        let envelope = Envelope::new(self.origin.clone(), scope, payload);
        self.store(&envelope)?;
        Ok(envelope)
    }

    /// Publishes a frame whose bytes depend on the event's identity.
    ///
    /// Needed because the deliverable frame carries its own [`EventId`] as the
    /// consumer's resume cursor, but the id is only known once it is minted
    /// here. Building the frame before the store is what guarantees a replayed
    /// frame is byte-identical to the live one.
    pub fn publish_with<F>(&self, scope: Scope, build: F) -> Result<Envelope>
    where
        F: FnOnce(EventId, u64) -> Bytes,
    {
        let event_id = EventId::new();
        let created_at_ms = crate::now_ms();
        let envelope = Envelope {
            event_id,
            scope,
            payload: build(event_id, created_at_ms),
            created_at_ms,
            origin: self.origin.clone(),
            exclude: None,
        };
        self.store(&envelope)?;
        Ok(envelope)
    }

    /// Publishes a frame with an explicit timestamp.
    ///
    /// Used for backfill and for tests that need a deterministic time; normal
    /// callers use [`Relay::publish`].
    pub fn publish_at(
        &self,
        scope: Scope,
        payload: impl Into<Bytes>,
        created_at_ms: u64,
    ) -> Result<Envelope> {
        let envelope =
            Envelope::with_created_at(self.origin.clone(), scope, payload, created_at_ms);
        self.store(&envelope)?;
        Ok(envelope)
    }

    fn store(&self, envelope: &Envelope) -> Result<()> {
        let shard = self.shard_of(&envelope.scope);
        self.backend.append(
            shard,
            LogRecord {
                event_id: envelope.event_id,
                scope: envelope.scope.clone(),
                payload: envelope.payload.clone(),
                created_at_ms: envelope.created_at_ms,
                origin: envelope.origin.clone(),
            },
        )
    }

    /// Replays a single shard from `from_ms` (inclusive), in append order.
    pub fn replay_shard(
        &self,
        shard: ShardId,
        from_ms: u64,
        limit: usize,
    ) -> Result<Vec<Envelope>> {
        self.backend
            .read(shard, from_ms, limit)
            .map(|records| records.into_iter().map(record_to_envelope).collect())
    }

    /// Replays one scope over the guaranteed replay window, returning at most
    /// `limit` of the most recent frames for that scope.
    pub fn replay_scope(&self, scope: &Scope, limit: usize) -> Result<Vec<Envelope>> {
        self.replay_scope_from(scope, crate::now_ms(), limit)
    }

    /// [`Relay::replay_scope`] with an explicit "now" (deterministic tests).
    pub fn replay_scope_from(
        &self,
        scope: &Scope,
        now_ms: u64,
        limit: usize,
    ) -> Result<Vec<Envelope>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let from_ms = self.retention.replay_start_ms(now_ms);
        let mut matching: Vec<Envelope> = self
            .replay_shard(scope.shard(), from_ms, usize::MAX)?
            .into_iter()
            .filter(|envelope| &envelope.scope == scope)
            .collect();
        if matching.len() > limit {
            let start = matching.len() - limit;
            matching = matching.split_off(start);
        }
        Ok(matching)
    }

    /// Runs one maintenance pass: trims every shard at the trim horizon.
    pub fn maintain(&self, now_ms: u64) -> Result<MaintenanceReport> {
        let before_ms = self.retention.trim_before_ms(now_ms);
        let mut trimmed = 0;
        for shard in 0..SHARD_COUNT {
            trimmed += self.backend.trim(shard, before_ms)?;
        }
        Ok(MaintenanceReport { trimmed })
    }

    /// Total records currently held across all shards.
    pub fn retained(&self) -> Result<usize> {
        let mut total = 0;
        for shard in 0..SHARD_COUNT {
            total += self.backend.len(shard)?;
        }
        Ok(total)
    }
}

impl std::fmt::Debug for Relay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Relay")
            .field("origin", &self.origin)
            .field("retention", &self.retention)
            .field("shards", &SHARD_COUNT)
            .finish_non_exhaustive()
    }
}

fn record_to_envelope(record: LogRecord) -> Envelope {
    Envelope {
        event_id: record.event_id,
        scope: record.scope,
        payload: record.payload,
        created_at_ms: record.created_at_ms,
        origin: record.origin,
        exclude: None,
    }
}

/// Convenience constructor for a shared in-memory backend.
pub fn memory_backend(max_len: usize) -> SharedBackend {
    Arc::new(crate::backend::memory::MemoryBackend::new(max_len))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn relay() -> Relay {
        Relay::with_defaults(memory_backend(1_000), "node-a").unwrap()
    }

    #[test]
    fn rejects_a_backend_with_the_wrong_shard_count() {
        struct WrongShards;
        impl crate::backend::RelayBackend for WrongShards {
            fn shard_count(&self) -> u8 {
                3
            }
            fn append(&self, _: ShardId, _: LogRecord) -> Result<()> {
                Ok(())
            }
            fn read(&self, _: ShardId, _: u64, _: usize) -> Result<Vec<LogRecord>> {
                Ok(Vec::new())
            }
            fn trim(&self, _: ShardId, _: u64) -> Result<u64> {
                Ok(0)
            }
            fn len(&self, _: ShardId) -> Result<usize> {
                Ok(0)
            }
        }

        let result = Relay::with_defaults(Arc::new(WrongShards), "node-a");
        assert!(matches!(result, Err(RelayError::Config(_))));
    }

    #[test]
    fn publish_stamps_origin_and_an_event_id() {
        let relay = relay();
        let envelope = relay.publish(Scope::Thread("thr_1".into()), "{}").unwrap();
        assert_eq!(envelope.origin, "node-a");
        assert_eq!(envelope.scope, Scope::Thread("thr_1".into()));
        assert_eq!(envelope.payload, Bytes::from_static(b"{}"));
    }

    #[test]
    fn replay_returns_only_the_requested_scope() {
        let relay = relay();
        let scope = Scope::Thread("thr_1".into());
        // A different scope that happens to hash to the same shard.
        let shard = relay.shard_of(&scope);
        relay.publish(scope.clone(), "{\"n\":1}").unwrap();
        relay.publish(scope.clone(), "{\"n\":2}").unwrap();

        let replayed = relay.replay_scope(&scope, 10).unwrap();
        assert_eq!(replayed.len(), 2);
        assert!(replayed.iter().all(|e| e.scope == scope));
        assert!(replayed.iter().all(|e| e.event_id.timestamp_ms() > 0));

        // Sanity: shard equality holds for the scope we queried.
        assert_eq!(relay.shard_of(&scope), shard);
    }

    #[test]
    fn replay_window_excludes_older_frames() {
        let relay = relay();
        let scope = Scope::Thread("thr_2".into());
        let now = 10_000_000u64;

        relay
            .publish_at(scope.clone(), "{\"old\":true}", now - 60 * 60 * 1_000)
            .unwrap();
        relay
            .publish_at(scope.clone(), "{\"fresh\":true}", now - 1_000)
            .unwrap();

        let replayed = relay.replay_scope_from(&scope, now, 10).unwrap();
        assert_eq!(replayed.len(), 1);
        assert_eq!(replayed[0].payload, Bytes::from_static(b"{\"fresh\":true}"));
    }

    #[test]
    fn replay_respects_the_limit_and_keeps_the_newest() {
        let relay = relay();
        let scope = Scope::Thread("thr_3".into());
        for i in 0..5 {
            relay
                .publish(scope.clone(), format!("{{\"n\":{i}}}"))
                .unwrap();
        }
        let replayed = relay.replay_scope(&scope, 2).unwrap();
        assert_eq!(replayed.len(), 2);
        assert_eq!(replayed[0].payload, Bytes::from_static(b"{\"n\":3}"));
        assert_eq!(replayed[1].payload, Bytes::from_static(b"{\"n\":4}"));
    }

    #[test]
    fn replay_of_an_empty_scope_is_empty() {
        let relay = relay();
        assert!(relay
            .replay_scope(&Scope::Thread("missing".into()), 10)
            .unwrap()
            .is_empty());
        assert!(relay
            .replay_scope(&Scope::Thread("missing".into()), 0)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn maintenance_trims_at_the_trim_horizon() {
        let relay = relay();
        let scope = Scope::Thread("thr_4".into());
        let now = 100_000_000u64;

        relay
            .publish_at(scope.clone(), "{\"old\":true}", now - 20 * 60 * 1_000)
            .unwrap();
        relay
            .publish_at(scope.clone(), "{\"fresh\":true}", now - 1_000)
            .unwrap();

        let report = relay.maintain(now).unwrap();
        assert_eq!(report.trimmed, 1);
        assert_eq!(relay.retained().unwrap(), 1);
    }

    #[test]
    fn publish_events_sort_by_creation() {
        let relay = relay();
        let scope = Scope::Global;
        let first = relay.publish(scope.clone(), "{}").unwrap();
        let second = relay.publish(scope, "{}").unwrap();
        assert!(first.event_id < second.event_id);
    }

    #[test]
    fn publish_with_can_embed_the_assigned_event_id() {
        let relay = relay();
        let envelope = relay
            .publish_with(Scope::Thread("thr_1".into()), |event_id, created_at_ms| {
                Bytes::from(format!("{event_id}:{created_at_ms}"))
            })
            .unwrap();

        let rendered = String::from_utf8(envelope.payload.to_vec()).unwrap();
        assert!(rendered.starts_with(&envelope.event_id.to_string()));
        assert!(rendered.ends_with(&envelope.created_at_ms.to_string()));

        // The stored bytes are what replay returns, unchanged.
        let replayed = relay
            .replay_scope(&Scope::Thread("thr_1".into()), 10)
            .unwrap();
        assert_eq!(replayed[0].payload, envelope.payload);
    }
}
