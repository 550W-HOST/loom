//! Shared Redis backend behaviour.
//!
//! The relay-level contract lives in `tests/relay.rs` and runs over every
//! backend. This file covers what only the shared backend can demonstrate:
//! that the log really is in Redis and outlives the process that wrote it,
//! that two nodes append to one ordered stream, and that the idempotence
//! primitive suppresses the duplicate a replay produces.
//!
//! Set `LOOM_REDIS_URL` to a reachable `redis://` URL to run these. Without it
//! they skip, so an unconfigured `cargo test` stays green and needs no service.
//! To run them locally:
//!
//! ```bash
//! docker run -d --rm --name loom-test-redis -p 127.0.0.1:6379:6379 redis:7.4
//! LOOM_REDIS_URL=redis://127.0.0.1:6379 cargo test -p loom-relay --test redis
//! ```

use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use loom_relay::backend::redis::{RedisBackend, RedisConfig};
use loom_relay::dedup::SeenSet;
use loom_relay::{EventId, Relay, Scope};

/// A Redis plus a key prefix unique to one test, cleaned up on drop.
struct Shared {
    url: String,
    prefix: String,
}

impl Shared {
    fn new() -> Option<Self> {
        let url = std::env::var("LOOM_REDIS_URL")
            .ok()
            .filter(|url| !url.trim().is_empty())?;
        let prefix = unique_prefix();
        let mut config = RedisConfig::from_url(&url).ok()?;
        config.key_prefix = prefix.clone();
        // Probe once, so an unreachable broker skips rather than fails.
        RedisBackend::open(config, 8).ok()?;
        Some(Self { url, prefix })
    }

    fn backend(&self, max_len: usize) -> RedisBackend {
        let mut config = RedisConfig::from_url(&self.url).expect("validated at construction");
        config.key_prefix = self.prefix.clone();
        RedisBackend::open(config, max_len).expect("validated at construction")
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        let _ = RedisBackend::open(self.config(), 8).map(|backend| backend.purge());
    }
}

impl Shared {
    fn config(&self) -> RedisConfig {
        let mut config = RedisConfig::from_url(&self.url).expect("validated at construction");
        config.key_prefix = self.prefix.clone();
        config
    }
}

/// Skips the test with a message when no Redis is configured.
macro_rules! shared {
    () => {
        match Shared::new() {
            Some(shared) => shared,
            None => {
                eprintln!("skipping: set LOOM_REDIS_URL to a reachable redis:// URL");
                return;
            }
        }
    };
}

fn unique_prefix() -> String {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let micros = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_micros())
        .unwrap_or(0);
    format!("loom:test:{}:{micros:x}:{counter}", std::process::id())
}

/// The point of the shared backend: a fresh process reads what a dead one
/// wrote, so a restart is not a lost window.
#[test]
fn a_fresh_backend_replays_the_shared_window() {
    let shared = shared!();
    let scope = Scope::Thread("thr_shared".into());
    let now = 1_000_000_000u64;

    let published = {
        let backend = Arc::new(shared.backend(1_000));
        let relay = Relay::with_defaults(backend, "node-a").unwrap();
        let first = relay
            .publish_at(scope.clone(), "{\"n\":1}", now - 2_000)
            .unwrap();
        let second = relay
            .publish_at(scope.clone(), "{\"n\":2}", now - 1_000)
            .unwrap();
        vec![first, second]
        // The process "dies" here: both the relay and its connections drop.
    };

    // A new process attaches to the same Redis and the same key prefix.
    let backend = Arc::new(shared.backend(1_000));
    let relay = Relay::with_defaults(backend, "node-b").unwrap();
    let replayed = relay.replay_scope_from(&scope, now, 10).unwrap();

    assert_eq!(replayed.len(), 2, "the shared window did not survive");
    for (restored, original) in replayed.iter().zip(published.iter()) {
        assert_eq!(restored.event_id, original.event_id);
        assert_eq!(restored.payload, original.payload);
        assert_eq!(
            restored, original,
            "a replayed frame must be byte-identical"
        );
    }
}

/// Replay is normal, so a consumer must drop the second copy of an event it
/// already delivered. That is the `EventId` contract, exercised end to end.
#[test]
fn duplicate_delivery_is_deduplicated_by_event_id() {
    let shared = shared!();
    let relay = Relay::with_defaults(Arc::new(shared.backend(100)), "node-a").unwrap();
    let scope = Scope::Thread("thr_dedup".into());

    let live = relay.publish(scope.clone(), "{\"n\":1}").unwrap();
    let replayed = relay.replay_scope(&scope, 10).unwrap();
    assert_eq!(replayed.len(), 1);
    assert_eq!(replayed[0].event_id, live.event_id);

    // The same frame arrives twice: once live, once from the log.
    let mut seen = SeenSet::new(64);
    let mut delivered = 0;
    for envelope in std::iter::once(&live).chain(replayed.iter()) {
        if seen.insert(envelope.event_id) {
            delivered += 1;
        }
    }
    assert_eq!(delivered, 1, "the replay must be recognised as a duplicate");
}

/// Two server nodes publish and replay through one log; the event ids stay
/// globally unique and the append order is shared.
#[test]
fn two_nodes_append_to_one_ordered_log() {
    let shared = shared!();
    let scope = Scope::Thread("thr_two_nodes".into());
    let node_a = Relay::with_defaults(Arc::new(shared.backend(1_000)), "node-a").unwrap();
    let node_b = Relay::with_defaults(Arc::new(shared.backend(1_000)), "node-b").unwrap();

    let first = node_a.publish(scope.clone(), "{\"node\":\"a\"}").unwrap();
    let second = node_b.publish(scope.clone(), "{\"node\":\"b\"}").unwrap();
    let third = node_a.publish(scope.clone(), "{\"node\":\"a\"}").unwrap();

    for relay in [&node_a, &node_b] {
        let replayed = relay.replay_scope(&scope, 10).unwrap();
        let ids: Vec<EventId> = replayed.iter().map(|envelope| envelope.event_id).collect();
        assert_eq!(
            ids,
            vec![first.event_id, second.event_id, third.event_id],
            "{} saw a different order",
            relay.origin()
        );
    }

    let unique: HashSet<EventId> = [first.event_id, second.event_id, third.event_id]
        .into_iter()
        .collect();
    assert_eq!(unique.len(), 3, "event ids must be unique across nodes");
    assert!(first.event_id < second.event_id && second.event_id < third.event_id);
}

/// Purging drops the whole window, which is what test teardown and a
/// deliberate "start over" both need.
#[test]
fn purge_drops_the_whole_window() {
    let shared = shared!();
    let scope = Scope::Thread("thr_purge".into());
    let relay = Relay::with_defaults(Arc::new(shared.backend(100)), "node-a").unwrap();
    relay.publish(scope.clone(), "{}").unwrap();
    assert_eq!(relay.replay_scope(&scope, 10).unwrap().len(), 1);

    shared.backend(100).purge().unwrap();

    let relay = Relay::with_defaults(Arc::new(shared.backend(100)), "node-a").unwrap();
    assert!(relay.replay_scope(&scope, 10).unwrap().is_empty());
}

/// A wrong address fails at construction instead of on the first event.
#[test]
fn open_fails_fast_when_redis_is_unreachable() {
    // Reserve an ephemeral port and release it, so the connect is refused.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);

    let mut config = RedisConfig::new("127.0.0.1", port);
    config.connect_timeout = Duration::from_millis(500);
    assert!(
        RedisBackend::open(config, 100).is_err(),
        "opening against a dead broker must fail, not defer"
    );
}
