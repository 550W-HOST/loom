//! End-to-end behaviour of the relay layer through its public API.
//!
//! Every scenario runs over every available backend: the in-process
//! `MemoryBackend` and the durable `DiskBackend`. The relay facade, retention
//! policy and replay semantics must be indistinguishable between them, so the
//! only thing that changes is which storage the `Relay` is built over. That is
//! the backend contract test: adding a backend means adding a case here, not
//! writing a parallel suite.

use bytes::Bytes;
use loom_relay::backend::disk::DiskBackend;
use loom_relay::backend::memory::MemoryBackend;
use loom_relay::backend::SharedBackend;
use loom_relay::dedup::SeenSet;
use loom_relay::retention::Retention;
use loom_relay::{now_ms, Envelope, Relay, Scope, WireEnvelope, SHARD_COUNT};
use std::sync::Arc;
use tempfile::TempDir;

/// The backends every scenario is exercised against.
#[derive(Clone, Debug, PartialEq, Eq)]
enum BackendKind {
    Memory,
    Disk,
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
        }
    }

    /// Whether the storage outlives the process, so a "restart" can replay.
    fn is_durable(&self) -> bool {
        !matches!(self.kind, BackendKind::Memory)
    }
}

fn cases() -> Vec<Case> {
    [BackendKind::Memory, BackendKind::Disk]
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

/// History is not the replay window.
///
/// The window is what a reader that has just attached is *guaranteed*; the
/// retained log is what the backend still holds. Reading a thread's timeline
/// through the window is what made a conversation disappear from it five
/// minutes after the last event, while every record was still there.
#[test]
fn the_retained_log_outlives_the_replay_window() {
    for case in cases() {
        let relay = case.relay(100);
        let scope = Scope::Thread("thr_history".into());
        let now = 1_000_000u64;

        // Ten minutes old: well outside the five-minute default grace window,
        // and still inside the backend's per-shard cap.
        relay
            .publish_at(scope.clone(), "{\"old\":1}", now - 600_000)
            .unwrap();
        relay
            .publish_at(scope.clone(), "{\"recent\":1}", now - 10)
            .unwrap();

        let windowed = relay.replay_scope_from(&scope, now, 10).unwrap();
        assert_eq!(
            windowed.len(),
            1,
            "the window keeps only what it guarantees for {:?}",
            case.kind
        );

        let retained = relay.retained_scope(&scope, 10).unwrap();
        assert_eq!(
            retained.len(),
            2,
            "the retained log still holds the history for {:?}",
            case.kind
        );
        assert_eq!(retained[0].payload, Bytes::from_static(b"{\"old\":1}"));
        assert_eq!(retained[1].payload, Bytes::from_static(b"{\"recent\":1}"));

        // Like the windowed read, it is a tail: a limit keeps the newest.
        let tail = relay.retained_scope(&scope, 1).unwrap();
        assert_eq!(tail.len(), 1);
        assert_eq!(tail[0].payload, Bytes::from_static(b"{\"recent\":1}"));
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

// ---------------------------------------------------------------------------
// Forward pagination
//
// A resuming consumer advances its cursor to the last frame it received. These
// scenarios pin the properties that make that safe: a page always moves
// forward, `has_more` is exact, and no frame is skipped — over every backend.
// ---------------------------------------------------------------------------

#[test]
fn a_page_returns_the_oldest_frames_after_the_cursor() {
    for case in cases() {
        let relay = case.relay(1_000);
        let scope = Scope::Thread("thr_page".into());
        for i in 0..10 {
            relay
                .publish(scope.clone(), format!("{{\"n\":{i}}}"))
                .unwrap();
        }

        let first = relay.replay_page_after(&scope, None, now_ms(), 4).unwrap();
        assert_eq!(first.events.len(), 4);
        assert!(first.has_more);
        assert_eq!(first.events[0].payload, Bytes::from_static(b"{\"n\":0}"));
        assert_eq!(first.events[3].payload, Bytes::from_static(b"{\"n\":3}"));

        let second = relay
            .replay_page_after(&scope, first.next_cursor(), now_ms(), 4)
            .unwrap();
        assert_eq!(second.events[0].payload, Bytes::from_static(b"{\"n\":4}"));
        assert_eq!(second.events[3].payload, Bytes::from_static(b"{\"n\":7}"));
        assert!(second.has_more);

        let last = relay
            .replay_page_after(&scope, second.next_cursor(), now_ms(), 4)
            .unwrap();
        assert_eq!(last.events.len(), 2);
        assert_eq!(last.events[0].payload, Bytes::from_static(b"{\"n\":8}"));
        assert!(!last.has_more, "the final page is short and says so");

        // Contrast: the tail view ignores the cursor and keeps the newest
        // frames, which is what a client opening the scope wants.
        let tail = relay.replay_scope(&scope, 4).unwrap();
        assert_eq!(tail.len(), 4);
        assert_eq!(tail[0].payload, Bytes::from_static(b"{\"n\":6}"));
    }
}

/// The point of forward paging: a reconnect that missed more frames than one
/// page must still recover every one of them.
#[test]
fn paging_from_a_cursor_recovers_every_missed_frame() {
    for case in cases() {
        let relay = case.relay(10_000);
        let scope = Scope::Thread("thr_missed".into());

        let mut ids = Vec::new();
        for i in 0..50 {
            ids.push(
                relay
                    .publish(scope.clone(), format!("{{\"n\":{i}}}"))
                    .unwrap()
                    .event_id,
            );
        }

        // Resume just after the second frame, with a page size far below the
        // backlog: 48 frames remain, 5 per page.
        let mut cursor = Some(ids[1]);
        let mut seen = Vec::new();
        let mut pages = 0;
        loop {
            let page = relay
                .replay_page_after(&scope, cursor, now_ms(), 5)
                .unwrap();
            assert!(page.events.len() <= 5, "the page limit is honoured");
            let last = page.next_cursor();
            seen.extend(page.events.iter().map(|envelope| envelope.event_id));
            pages += 1;
            if !page.has_more {
                break;
            }
            cursor = last;
            assert!(pages < 100, "paging must terminate");
        }

        assert_eq!(seen.len(), 48, "every frame after the cursor must arrive");
        assert_eq!(
            seen,
            ids[2..].to_vec(),
            "in order, with no gap and no repeat"
        );
    }
}

/// `has_more` is exact, not "the page happened to be full".
#[test]
fn has_more_is_exact_at_the_boundary() {
    for case in cases() {
        let relay = case.relay(1_000);
        let scope = Scope::Thread("thr_exact".into());
        for i in 0..4 {
            relay
                .publish(scope.clone(), format!("{{\"n\":{i}}}"))
                .unwrap();
        }

        let exact = relay.replay_page_after(&scope, None, now_ms(), 4).unwrap();
        assert_eq!(exact.events.len(), 4);
        assert!(
            !exact.has_more,
            "a page that exactly consumed the log is done"
        );

        let short = relay.replay_page_after(&scope, None, now_ms(), 5).unwrap();
        assert_eq!(short.events.len(), 4);
        assert!(!short.has_more);

        let more = relay.replay_page_after(&scope, None, now_ms(), 3).unwrap();
        assert_eq!(more.events.len(), 3);
        assert!(more.has_more);
    }
}

#[test]
fn a_page_filters_by_scope_and_stays_inside_the_window() {
    for case in cases() {
        let relay = case.relay(10_000);
        let scope = Scope::Thread("thr_mine".into());
        // Enough other scopes to guarantee shard sharing.
        for i in 0..64 {
            relay
                .publish(Scope::Thread(format!("thr_other_{i}")), "{\"other\":true}")
                .unwrap();
        }
        for i in 0..6 {
            relay
                .publish(scope.clone(), format!("{{\"n\":{i}}}"))
                .unwrap();
        }

        let page = relay
            .replay_page_after(&scope, None, now_ms(), 100)
            .unwrap();
        assert_eq!(page.events.len(), 6);
        assert!(page.events.iter().all(|envelope| envelope.scope == scope));
        assert!(!page.has_more);

        // A frame outside the replay window is not paged back. This uses its
        // own scope because `case` owns one storage that a second `relay()`
        // would reopen — a durable backend sees the same log again.
        let windowed_scope = Scope::Thread("thr_window".into());
        let now = now_ms();
        let stale = case.relay(100);
        stale
            .publish_at(windowed_scope.clone(), "{\"old\":true}", now - 3_600_000)
            .unwrap();
        stale
            .publish_at(windowed_scope.clone(), "{\"new\":true}", now)
            .unwrap();
        let windowed = stale
            .replay_page_after(&windowed_scope, None, now, 100)
            .unwrap();
        assert_eq!(windowed.events.len(), 1);
        assert_eq!(
            windowed.events[0].payload,
            Bytes::from_static(b"{\"new\":true}")
        );
    }
}

#[test]
fn an_empty_page_reports_no_more() {
    for case in cases() {
        let relay = case.relay(100);
        let scope = Scope::Thread("thr_empty".into());

        let empty = relay.replay_page_after(&scope, None, now_ms(), 10).unwrap();
        assert!(empty.events.is_empty());
        assert!(!empty.has_more);

        let published = relay.publish(scope.clone(), "{}").unwrap();
        let after = relay
            .replay_page_after(&scope, Some(published.event_id), now_ms(), 10)
            .unwrap();
        assert!(after.events.is_empty());
        assert!(!after.has_more);
    }
}
