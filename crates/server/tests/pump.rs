//! Reader-progress guarantees.
//!
//! The pump reads from the log through a cursor and forwards to the hub. The
//! property these tests defend is that forwarding always makes progress: a
//! reader that has delivered N events must never be able to stall on events it
//! has already delivered, no matter how they are distributed in time.

use std::sync::Arc;
use std::time::Duration;

use loom_relay::backend::memory::MemoryBackend;
use loom_relay::{Relay, Scope, SharedBackend};
use loom_server::hub_actor::HubHandle;
use loom_server::pump::{Pump, PumpConfig};
use loom_server::transport::ChannelTransport;

/// Drains up to `expected` frames, or until `window` elapses.
async fn collect(
    rx: &mut tokio::sync::mpsc::Receiver<bytes::Bytes>,
    expected: usize,
    window: Duration,
) -> usize {
    let deadline = tokio::time::Instant::now() + window;
    let mut got = 0;
    while got < expected {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => break,
            frame = rx.recv() => match frame {
                Some(_) => got += 1,
                None => break,
            },
        }
    }
    got
}

/// Regression: a burst larger than `batch_limit` minted inside one millisecond
/// used to pin the reader permanently.
///
/// The reader bounded its read by the cursor's *millisecond* and then took
/// `limit` records from the front of that window. Once the cursor reached the
/// end of a same-millisecond run longer than `limit`, every subsequent read
/// returned the same already-delivered prefix, forwarded nothing, and never
/// advanced — so every later event on that shard was silently undelivered
/// until the records aged out. The cursor now travels into the backend, so a
/// batch can never consist solely of records the cursor has passed.
#[tokio::test]
async fn a_burst_beyond_the_batch_limit_in_one_millisecond_is_fully_delivered() {
    let backend: SharedBackend = Arc::new(MemoryBackend::new(100_000));
    let relay = Relay::with_defaults(backend, "test").unwrap();

    let scope = Scope::Thread("thr_burst".into());
    let same_ms = 1_700_000_000_000u64;
    const COUNT: usize = 32;
    for i in 0..COUNT {
        // `publish_at` pins every record to the same millisecond.
        relay
            .publish_at(scope.clone(), format!("{{\"n\":{i}}}"), same_ms)
            .unwrap();
    }

    let (hub, _actor) = HubHandle::spawn(1_024);
    let (transport, mut rx) = ChannelTransport::new(4_096);
    hub.connect(Box::new(transport), scope.clone())
        .await
        .unwrap();

    // A batch limit far below the burst: the failing case.
    let pump = Pump::spawn(
        relay.clone(),
        hub.clone(),
        PumpConfig {
            batch_limit: 4,
            safety_tick: Duration::from_millis(20),
        },
    );

    let delivered = collect(&mut rx, COUNT, Duration::from_secs(3)).await;
    pump.stop();

    assert_eq!(
        delivered, COUNT,
        "every record in a same-millisecond burst must be delivered"
    );
}

/// The reader must keep consuming a burst that spans many batches, and the
/// frames must arrive in publication order.
#[tokio::test]
async fn a_long_burst_arrives_in_order_across_batches() {
    let backend: SharedBackend = Arc::new(MemoryBackend::new(100_000));
    let relay = Relay::with_defaults(backend, "test").unwrap();

    let scope = Scope::Thread("thr_long".into());
    const COUNT: usize = 200;
    for i in 0..COUNT {
        relay
            .publish(scope.clone(), format!("{{\"n\":{i}}}"))
            .unwrap();
    }

    let (hub, _actor) = HubHandle::spawn(1_024);
    let (transport, mut rx) = ChannelTransport::new(4_096);
    hub.connect(Box::new(transport), scope.clone())
        .await
        .unwrap();

    let pump = Pump::spawn(
        relay.clone(),
        hub.clone(),
        PumpConfig {
            batch_limit: 8,
            safety_tick: Duration::from_millis(20),
        },
    );

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut payloads = Vec::new();
    while payloads.len() < COUNT {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => break,
            frame = rx.recv() => match frame {
                Some(frame) => {
                    // This test publishes straight through `Relay`, so the
                    // stored bytes are the raw payload rather than a
                    // client-facing event frame (that wrapping belongs to the
                    // protocol layer, exercised in `ws.rs`).
                    payloads.push(String::from_utf8(frame.to_vec()).unwrap());
                }
                None => break,
            },
        }
    }
    pump.stop();

    assert_eq!(payloads.len(), COUNT);
    let expected: Vec<String> = (0..COUNT).map(|i| format!("{{\"n\":{i}}}")).collect();
    assert_eq!(payloads, expected);
}

/// Envelopes published after a reader is already caught up must still arrive;
/// the cursor is not a one-way door.
#[tokio::test]
async fn events_published_after_the_reader_catches_up_are_delivered() {
    let backend: SharedBackend = Arc::new(MemoryBackend::new(100_000));
    let relay = Relay::with_defaults(backend, "test").unwrap();
    let scope = Scope::Thread("thr_live".into());

    relay.publish(scope.clone(), "{\"n\":0}").unwrap();

    let (hub, _actor) = HubHandle::spawn(1_024);
    let (transport, mut rx) = ChannelTransport::new(4_096);
    hub.connect(Box::new(transport), scope.clone())
        .await
        .unwrap();

    let pump = Pump::spawn(
        relay.clone(),
        hub.clone(),
        PumpConfig {
            batch_limit: 4,
            safety_tick: Duration::from_millis(20),
        },
    );

    assert_eq!(collect(&mut rx, 1, Duration::from_secs(2)).await, 1);

    for i in 1..6 {
        relay
            .publish(scope.clone(), format!("{{\"n\":{i}}}"))
            .unwrap();
    }
    let more = collect(&mut rx, 5, Duration::from_secs(2)).await;
    pump.stop();
    assert_eq!(more, 5);
}
