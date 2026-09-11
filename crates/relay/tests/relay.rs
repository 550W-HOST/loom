//! End-to-end behaviour of the relay layer through its public API.
//!
//! Every scenario runs over every available backend: the in-process
//! `MemoryBackend`, the durable `DiskBackend`, and — when `LOOM_REDIS_URL`
//! points at a reachable Redis — the shared `RedisBackend`. The relay facade,
//! retention policy and replay semantics must be indistinguishable between
//! them, so the only thing that changes is which storage the `Relay` is built
//! over. That is the backend contract test: adding a backend means adding a
//! case here, not writing a parallel suite.

use bytes::Bytes;
use loom_relay::backend::disk::DiskBackend;
use loom_relay::backend::memory::MemoryBackend;
use loom_relay::backend::redis::{RedisBackend, RedisConfig};
use loom_relay::backend::SharedBackend;
use loom_relay::dedup::SeenSet;
use loom_relay::retention::Retention;
use loom_relay::{now_ms, Envelope, Relay, Scope, WireEnvelope, SHARD_COUNT};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tempfile::TempDir;

/// The backends every scenario is exercised against.
#[derive(Clone, Debug, PartialEq, Eq)]
enum BackendKind {
    Memory,
    Disk,
    /// A shared Redis, with the key prefix unique to one test case.
    Redis {
        url: String,
        prefix: String,
    },
}

/// One test run: a backend choice plus the scratch directory a disk backend
/// owns (unused, and harmless, for the other cases).
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
        match &self.kind {
            BackendKind::Memory => Arc::new(MemoryBackend::new(max_len)),
            BackendKind::Disk => Arc::new(DiskBackend::open(self.dir.path(), max_len).unwrap()),
            BackendKind::Redis { url, prefix } => {
                let mut config = RedisConfig::from_url(url).unwrap();
                config.key_prefix = prefix.clone();
                Arc::new(RedisBackend::open(config, max_len).unwrap())
            }
        }
    }

    /// Whether the storage outlives the process, so a "restart" can replay.
    fn is_durable(&self) -> bool {
        !matches!(self.kind, BackendKind::Memory)
    }
}

impl Drop for Case {
    fn drop(&mut self) {
        if let BackendKind::Redis { url, prefix } = &self.kind {
            // Best-effort: a test must not fail because cleanup could not
            // reach Redis after the assertions already ran.
            if let Ok(mut config) = RedisConfig::from_url(url) {
                config.key_prefix = prefix.clone();
                if let Ok(backend) = RedisBackend::open(config, 8) {
                    let _ = backend.purge();
                }
            }
        }
    }
}

fn cases() -> Vec<Case> {
    let mut kinds = vec![BackendKind::Memory, BackendKind::Disk];
    match redis_case_kind() {
        Some(kind) => kinds.push(kind),
        None => eprintln!(
            "loom-relay: skipping the Redis backend cases; set LOOM_REDIS_URL to a reachable redis:// URL to run them"
        ),
    }
    kinds
        .into_iter()
        .map(|kind| Case {
            kind,
            dir: TempDir::new().unwrap(),
        })
        .collect()
}

/// The subset of cases whose log survives the process.
fn durable_cases() -> Vec<Case> {
    cases().into_iter().filter(Case::is_durable).collect()
}

/// Detects a usable shared Redis once, so an unconfigured run still passes.
fn redis_case_kind() -> Option<BackendKind> {
    let url = std::env::var("LOOM_REDIS_URL").ok()?;
    let prefix = unique_prefix();
    let mut config = RedisConfig::from_url(&url).ok()?;
    config.key_prefix = prefix.clone();
    match RedisBackend::open(config, 8) {
        Ok(_) => Some(BackendKind::Redis { url, prefix }),
        Err(error) => {
            eprintln!("loom-relay: LOOM_REDIS_URL is set but unusable: {error}");
            None
        }
    }
}

/// A key prefix no other test, run or machine sharing the Redis will use.
fn unique_prefix() -> String {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let micros = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_micros())
        .unwrap_or(0);
    format!("loom:test:{}:{micros:x}:{counter}", std::process::id())
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

/// The property the durable backends exist for: a write, a clean shutdown, a
/// fresh process over the same storage, and the grace window still replays.
#[test]
fn replay_survives_a_restart() {
    for case in durable_cases() {
        let scope = Scope::Thread("thr_restart".into());
        let now = 1_000_000_000u64;

        let published = {
            let relay = case.relay(1_000);
            let first = relay
                .publish_at(scope.clone(), "{\"n\":1}", now - 1_000)
                .unwrap();
            let second = relay
                .publish_at(scope.clone(), "{\"n\":2}", now - 500)
                .unwrap();
            vec![first, second]
            // The relay and its backend are dropped here, flushing the log.
        };

        // A new process attaches to the same storage.
        let relay = case.relay(1_000);
        let replayed = relay.replay_scope_from(&scope, now, 10).unwrap();
        assert_eq!(
            replayed.len(),
            2,
            "restart replay was incomplete for {:?}",
            case.kind
        );
        assert_eq!(replayed[0].event_id, published[0].event_id);
        assert_eq!(replayed[0].payload, published[0].payload);
        assert_eq!(replayed[1].event_id, published[1].event_id);
        assert_eq!(replayed[1].payload, published[1].payload);

        // Frames replayed after a restart are byte-identical to the live ones.
        for (replayed_frame, live_frame) in replayed.iter().zip(published.iter()) {
            assert_eq!(replayed_frame, live_frame);
        }
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
