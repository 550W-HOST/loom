//! End-to-end behaviour of the relay layer through its public API.

use bytes::Bytes;
use loom_relay::backend::memory::MemoryBackend;
use loom_relay::backend::SharedBackend;
use loom_relay::dedup::SeenSet;
use loom_relay::retention::Retention;
use loom_relay::{now_ms, Envelope, Relay, Scope, WireEnvelope, SHARD_COUNT};
use std::sync::Arc;

fn relay(max_len: usize) -> Relay {
    let backend: SharedBackend = Arc::new(MemoryBackend::new(max_len));
    Relay::with_defaults(backend, "node-a").unwrap()
}

#[test]
fn publish_replay_trim_cycle() {
    let relay = relay(100);
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

#[test]
fn scopes_share_shards_without_leaking_into_each_other() {
    let relay = relay(100);
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

#[test]
fn a_reconnecting_consumer_replays_only_what_it_missed() {
    let relay = relay(1_000);
    let scope = Scope::Thread("thr_1".into());

    let before = now_ms();
    let first = relay.publish(scope.clone(), "{\"n\":1}").unwrap();
    assert!(first.event_id.timestamp_ms() >= before);

    // A consumer that was connected, then went away, then came back asks for
    // the grace window and deduplicates what it already had.
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

#[test]
fn wire_form_survives_a_json_hop() {
    let relay = relay(100);
    let scope = Scope::Host("host_1".into());
    let envelope = relay.publish(scope, "{\"kind\":\"task\"}").unwrap();

    let text = serde_json::to_string(&envelope.to_wire()).unwrap();
    let decoded: WireEnvelope = serde_json::from_str(&text).unwrap();
    let restored = Envelope::try_from(decoded).unwrap();

    assert_eq!(restored, envelope);
}

#[test]
fn per_shard_cap_bounds_memory() {
    let relay = relay(8);
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

#[test]
fn custom_retention_is_honoured() {
    let backend: SharedBackend = Arc::new(MemoryBackend::new(100));
    let retention = Retention {
        replay_grace_ms: 1_000,
        trim_horizon_ms: 2_000,
        ttl_ms: 10_000,
        maintenance_interval_ms: 1_000,
        ..Retention::default()
    };
    let relay = Relay::new(backend, retention, "node-a").unwrap();

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
