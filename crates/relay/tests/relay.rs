//! End-to-end behaviour of the relay layer through its public API.
//!
//! Every scenario runs twice: once over the in-process `MemoryBackend` and
//! once over the durable `DiskBackend`. The relay facade, retention policy and
//! replay semantics must be indistinguishable between them, so the only thing
//! that changes is which storage the `Relay` is built over.

use bytes::Bytes;
use loom_relay::backend::disk::DiskBackend;
use loom_relay::backend::memory::MemoryBackend;
use loom_relay::backend::SharedBackend;
use loom_relay::dedup::SeenSet;
use loom_relay::retention::Retention;
use loom_relay::{now_ms, Envelope, Relay, Scope, WireEnvelope, SHARD_COUNT};
use std::sync::Arc;
use tempfile::TempDir;

/// The two backends every scenario is exercised against.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BackendKind {
    Memory,
    Disk,
}

const ALL_BACKENDS: [BackendKind; 2] = [BackendKind::Memory, BackendKind::Disk];

/// One test run: a backend choice plus the scratch directory a disk backend
/// owns (unused, and harmless, for the memory case).
struct Case {
    kind: BackendKind,
    dir: TempDir,
}

impl Case {
    fn relay(&self, max_len: usize) -> Relay {
        self.relay_with(max_len, Retention::default())
    }

    fn relay_with(&self, max_len: usize, retention: Retention) -> Relay {
        Relay::new(self.backend(max_len), retention, "node-a").unwrap()
    }

    fn backend(&self, max_len: usize) -> SharedBackend {
        match self.kind {
            BackendKind::Memory => Arc::new(MemoryBackend::new(max_len)),
            BackendKind::Disk => Arc::new(DiskBackend::open(self.dir.path(), max_len).unwrap()),
        }
    }
}

fn cases() -> Vec<Case> {
    ALL_BACKENDS
        .iter()
        .map(|kind| Case {
            kind: *kind,
            dir: TempDir::new().unwrap(),
        })
        .collect()
}

#[test]
fn publish_replay_trim_cycle() {
    for case in cases() {
        let relay = case.relay(100);
        let scope = Scope::Thread("thr_1".into());

        let first = relay.publish(scope.clone(), "{\"n\":1}").unwrap();
        let second = relay.publish(scope.clone(), "{\"n\":2}").unwrap();
        assert!(first.event_id < second.event_id);

        let replayed = relay.replay_scope(&scope, 10).unwrap();
        assert_eq!(replayed.len(), 2);
        assert_eq!(replayed[0].payload, Bytes::from_static(b"{\"n\":1}"));
        assert_eq!(replayed[1].payload, Bytes::from_static(b"{\"n\":2}"));

        // Advance well past the trim horizon and reclaim.
        let report = relay.maintain(now_ms() + 60 * 60 * 1_000).unwrap();
        assert_eq!(report.trimmed, 2);
        assert!(relay.replay_scope(&scope, 10).unwrap().is_empty());
    }
}

#[test]
fn scopes_share_shards_without_leaking_into_each_other() {
    for case in cases() {
        let relay = case.relay(100);
        // Enough scopes that collisions within a shard are guaranteed.
        let scopes: Vec<Scope> = (0..64).map(|i| Scope::Thread(format!("thr_{i}"))).collect();

        for (i, scope) in scopes.iter().enumerate() {
            relay
                .publish(scope.clone(), format!("{{\"i\":{i}}}"))
                .unwrap();
        }

        // Every shard is used, and no scope sees another scope's frames.
        let mut used = std::collections::HashSet::new();
        for scope in &scopes {
            used.insert(relay.shard_of(scope));
            let replayed = relay.replay_scope(scope, 100).unwrap();
            assert_eq!(replayed.len(), 1);
            assert!(replayed.iter().all(|envelope| &envelope.scope == scope));
        }
        assert_eq!(used.len(), usize::from(SHARD_COUNT));
    }
}

#[test]
fn a_reconnecting_consumer_replays_only_what_it_missed() {
    for case in cases() {
        let relay = case.relay(1_000);
        let scope = Scope::Thread("thr_1".into());

        let before = now_ms();
        let first = relay.publish(scope.clone(), "{\"n\":1}").unwrap();
        assert!(first.event_id.timestamp_ms() >= before);

        // A consumer that was connected, then went away, then came back asks
        // for the grace window and deduplicates what it already had.
        let mut seen = SeenSet::new(128);
        assert!(seen.insert(first.event_id), "delivered while connected");

        let second = relay.publish(scope.clone(), "{\"n\":2}").unwrap();

        let replayed = relay.replay_scope(&scope, 100).unwrap();
        let fresh: Vec<&Envelope> = replayed
            .iter()
            .filter(|envelope| seen.insert(envelope.event_id))
            .collect();

        assert_eq!(fresh.len(), 1);
        assert_eq!(fresh[0].event_id, second.event_id);
    }
}

#[test]
fn wire_form_survives_a_json_hop() {
    for case in cases() {
        let relay = case.relay(100);
        let scope = Scope::Host("host_1".into());
        let envelope = relay.publish(scope, "{\"kind\":\"task\"}").unwrap();

        let text = serde_json::to_string(&envelope.to_wire()).unwrap();
        let decoded: WireEnvelope = serde_json::from_str(&text).unwrap();
        let restored = Envelope::try_from(decoded).unwrap();

        assert_eq!(restored, envelope);
    }
}

#[test]
fn per_shard_cap_bounds_memory() {
    for case in cases() {
        let relay = case.relay(8);
        let scope = Scope::Thread("thr_1".into());
        for i in 0..100 {
            relay
                .publish(scope.clone(), format!("{{\"n\":{i}}}"))
                .unwrap();
        }
        assert!(relay.retained().unwrap() <= 8 * usize::from(SHARD_COUNT));

        let replayed = relay.replay_scope(&scope, 100).unwrap();
        // Only the newest frames for this scope survive the cap.
        assert!(replayed.len() <= 8);
        let last = replayed.last().unwrap();
        assert_eq!(last.payload, Bytes::from_static(b"{\"n\":99}"));
    }
}

#[test]
fn custom_retention_is_honoured() {
    for case in cases() {
        let retention = Retention {
            replay_grace_ms: 1_000,
            trim_horizon_ms: 2_000,
            ttl_ms: 10_000,
            maintenance_interval_ms: 1_000,
            ..Retention::default()
        };
        let relay = case.relay_with(100, retention);

        let scope = Scope::Thread("thr_1".into());
        let now = 1_000_000u64;
        relay
            .publish_at(scope.clone(), "{\"old\":1}", now - 5_000)
            .unwrap();
        relay
            .publish_at(scope.clone(), "{\"new\":1}", now - 10)
            .unwrap();

        let replayed = relay.replay_scope_from(&scope, now, 10).unwrap();
        assert_eq!(replayed.len(), 1);
        assert_eq!(replayed[0].payload, Bytes::from_static(b"{\"new\":1}"));
    }
}

/// The property this whole backend exists for: a write, a clean shutdown, a
/// fresh process over the same directory, and the grace window still replays.
#[test]
fn replay_survives_a_restart() {
    let dir = TempDir::new().unwrap();
    let scope = Scope::Thread("thr_restart".into());
    let now = 1_000_000_000u64;

    let published = {
        let backend: SharedBackend = Arc::new(DiskBackend::open(dir.path(), 1_000).unwrap());
        let relay = Relay::with_defaults(backend, "node-a").unwrap();
        let first = relay
            .publish_at(scope.clone(), "{\"n\":1}", now - 1_000)
            .unwrap();
        let second = relay
            .publish_at(scope.clone(), "{\"n\":2}", now - 500)
            .unwrap();
        vec![first, second]
        // Both the relay and the backend are dropped here, flushing the log.
    };

    // A new process opens the same directory.
    let backend: SharedBackend = Arc::new(DiskBackend::open(dir.path(), 1_000).unwrap());
    let relay = Relay::with_defaults(backend, "node-a").unwrap();

    let replayed = relay.replay_scope_from(&scope, now, 10).unwrap();
    assert_eq!(replayed.len(), 2);
    assert_eq!(replayed[0].event_id, published[0].event_id);
    assert_eq!(replayed[0].payload, published[0].payload);
    assert_eq!(replayed[1].event_id, published[1].event_id);
    assert_eq!(replayed[1].payload, published[1].payload);

    // Frames replayed after a restart are byte-identical to the live ones.
    for (replayed_frame, live_frame) in replayed.iter().zip(published.iter()) {
        assert_eq!(replayed_frame, live_frame);
    }
}

/// An unflushed tail — the shape a crash leaves behind — must not corrupt the
/// records before it.
#[test]
fn a_crashed_restart_loses_at_most_the_tail() {
    let dir = TempDir::new().unwrap();
    let scope = Scope::Thread("thr_crash".into());

    {
        let backend: SharedBackend = Arc::new(DiskBackend::open(dir.path(), 1_000).unwrap());
        let relay = Relay::with_defaults(backend, "node-a").unwrap();
        for i in 0..4 {
            relay
                .publish(scope.clone(), format!("{{\"n\":{i}}}"))
                .unwrap();
        }
        // No explicit flush: relying on the backend's own durability on a
        // clean drop, then corrupting the tail below.
    }

    // Simulate a half-written record left by a crash.
    let path = dir.path().join(format!("shard-{}.log", scope.shard()));
    {
        use std::io::Write as _;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(b"\x4D\x4F\x4F\x4C\x01\x02").unwrap();
        file.sync_all().unwrap();
    }

    let backend: SharedBackend = Arc::new(DiskBackend::open(dir.path(), 1_000).unwrap());
    let relay = Relay::with_defaults(backend, "node-a").unwrap();
    let replayed = relay.replay_scope(&scope, 100).unwrap();
    assert_eq!(replayed.len(), 4);
    for (i, envelope) in replayed.iter().enumerate() {
        assert_eq!(envelope.payload, Bytes::from(format!("{{\"n\":{i}}}")));
    }
}
