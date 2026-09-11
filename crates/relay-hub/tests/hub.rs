//! Hub behaviour: subscription routing, idempotent delivery, backpressure.

use bb_relay::envelope::Envelope;
use bb_relay::event_id::EventId;
use bb_relay::scope::Scope;
use bb_relay_hub::{DeliveryReport, Hub, RecordingTransport, SubscribeOutcome};

fn envelope(scope: Scope, event_id: EventId, payload: &str) -> Envelope {
    Envelope {
        event_id,
        scope,
        payload: bytes::Bytes::copy_from_slice(payload.as_bytes()),
        created_at_ms: 1,
        origin: "node-a".into(),
        exclude: None,
    }
}

#[test]
fn delivers_only_to_subscribers_of_the_scope() {
    let mut hub = Hub::new();
    let thread = Scope::Thread("thr_1".into());
    let other = Scope::Thread("thr_2".into());

    let subscribed = RecordingTransport::new();
    let unrelated = RecordingTransport::new();
    hub.connect(Box::new(subscribed.clone()), thread.clone());
    hub.connect(Box::new(unrelated.clone()), other);

    let report = hub.deliver(&envelope(thread, EventId::new(), "{\"n\":1}"));

    assert_eq!(
        report,
        DeliveryReport {
            delivered: 1,
            duplicates: 0,
            dropped: 0
        }
    );
    assert_eq!(subscribed.frames(), vec!["{\"n\":1}".to_string()]);
    assert!(unrelated.is_empty());
}

#[test]
fn fans_out_to_several_subscribers() {
    let mut hub = Hub::new();
    let thread = Scope::Thread("thr_1".into());

    let a = RecordingTransport::new();
    let b = RecordingTransport::new();
    hub.connect(Box::new(a.clone()), thread.clone());
    hub.connect(Box::new(b.clone()), thread.clone());

    let event = EventId::new();
    let first = hub.deliver(&envelope(thread.clone(), event, "{\"n\":1}"));
    assert_eq!(first.delivered, 2);

    // Replay of the same event is suppressed per connection.
    let second = hub.deliver(&envelope(thread, event, "{\"n\":1}"));
    assert_eq!(
        second,
        DeliveryReport {
            delivered: 0,
            duplicates: 2,
            dropped: 0
        }
    );

    assert_eq!(a.frames().len(), 1);
    assert_eq!(b.frames().len(), 1);
}

#[test]
fn replay_after_reconnect_delivers_the_missed_frame_once() {
    let mut hub = Hub::new();
    let thread = Scope::Thread("thr_1".into());
    let transport = RecordingTransport::new();
    let id = hub.connect(Box::new(transport.clone()), thread.clone());

    let connected = EventId::new();
    hub.deliver(&envelope(thread.clone(), connected, "{\"connected\":true}"));

    // The consumer drops, then reconnects and replays the window.
    hub.disconnect(id);
    let transport = RecordingTransport::new();
    let id = hub.connect(Box::new(transport.clone()), thread.clone());

    let missed = EventId::new();
    let report = hub.deliver_all(vec![
        &envelope(thread.clone(), connected, "{\"connected\":true}"),
        &envelope(thread.clone(), missed, "{\"missed\":true}"),
    ]);

    // Both frames were in the replay window; the fresh connection accepts both
    // because its dedup history reset with the connection.
    assert_eq!(report.delivered, 2);
    assert_eq!(transport.frames().len(), 2);
    assert!(hub.is_connected(id));
}

#[test]
fn a_closed_transport_is_counted_as_dropped() {
    let mut hub = Hub::new();
    let thread = Scope::Thread("thr_1".into());
    let transport = RecordingTransport::new();
    let id = hub.connect(Box::new(transport.clone()), thread.clone());

    transport.close();

    let event = EventId::new();
    let report = hub.deliver(&envelope(thread.clone(), event, "{}"));
    assert_eq!(report.dropped, 1);
    assert_eq!(report.delivered, 0);

    // Dedup records the event before the transport is asked, so a retry is
    // suppressed as a duplicate rather than dropped a second time.
    let retry = hub.deliver(&envelope(thread, event, "{}"));
    assert_eq!(retry.duplicates, 1);
    assert_eq!(retry.dropped, 0);
    assert!(hub.is_connected(id));
}

#[test]
fn subscribe_reports_the_first_subscriber_edge() {
    let mut hub = Hub::new();
    let primary = Scope::User("user_1".into());
    let thread = Scope::Thread("thr_1".into());

    let a = hub.connect(Box::new(RecordingTransport::new()), primary.clone());
    let b = hub.connect(Box::new(RecordingTransport::new()), primary);

    assert_eq!(
        hub.subscribe(a, thread.clone()),
        SubscribeOutcome {
            newly_added: true,
            first_subscriber: true
        }
    );
    assert_eq!(
        hub.subscribe(b, thread.clone()),
        SubscribeOutcome {
            newly_added: true,
            first_subscriber: false
        }
    );
    // Idempotent re-subscribe.
    assert_eq!(
        hub.subscribe(b, thread.clone()),
        SubscribeOutcome::default()
    );
    assert_eq!(hub.subscriber_count(&thread), 2);
}

#[test]
fn subscribe_to_an_unknown_connection_is_a_no_op() {
    let mut hub = Hub::new();
    assert_eq!(
        hub.subscribe(999, Scope::Thread("thr_1".into())),
        SubscribeOutcome::default()
    );
}

#[test]
fn disconnect_cleans_rooms_and_reports_empty_scopes() {
    let mut hub = Hub::new();
    let thread = Scope::Thread("thr_1".into());
    let a = hub.connect(Box::new(RecordingTransport::new()), thread.clone());
    let b = hub.connect(Box::new(RecordingTransport::new()), thread.clone());
    assert_eq!(hub.subscriber_count(&thread), 2);
    assert_eq!(hub.room_count(), 1);

    assert!(hub.disconnect(a).is_empty(), "room still has b");
    assert_eq!(hub.disconnect(b), vec![thread.clone()]);
    assert_eq!(hub.room_count(), 0);
    assert_eq!(hub.connection_count(), 0);
    assert_eq!(hub.subscriber_count(&thread), 0);
}

#[test]
fn disconnect_of_an_unknown_connection_is_a_no_op() {
    let mut hub = Hub::new();
    assert!(hub.disconnect(42).is_empty());
}

#[test]
fn unsubscribe_removes_the_room_but_not_the_primary_scope() {
    let mut hub = Hub::new();
    let primary = Scope::User("user_1".into());
    let thread = Scope::Thread("thr_1".into());
    let id = hub.connect(Box::new(RecordingTransport::new()), primary.clone());

    hub.subscribe(id, thread.clone());
    assert!(hub.unsubscribe(id, &thread));
    assert_eq!(hub.room_count(), 1, "primary room remains");
    assert!(!hub.unsubscribe(id, &thread), "already removed");

    assert!(
        !hub.unsubscribe(id, &primary),
        "a connection cannot leave its primary scope"
    );
    assert_eq!(hub.subscriber_count(&primary), 1);
}

#[test]
fn delivering_to_an_empty_scope_is_a_no_op() {
    let mut hub = Hub::new();
    hub.connect(
        Box::new(RecordingTransport::new()),
        Scope::User("user_1".into()),
    );
    let report = hub.deliver(&envelope(
        Scope::Thread("nobody".into()),
        EventId::new(),
        "{}",
    ));
    assert_eq!(report, DeliveryReport::default());
    assert_eq!(report.considered(), 0);
}

#[test]
fn reset_dedup_makes_a_replay_visible_again() {
    let mut hub = Hub::new();
    let thread = Scope::Thread("thr_1".into());
    let transport = RecordingTransport::new();
    let id = hub.connect(Box::new(transport.clone()), thread.clone());

    let event = EventId::new();
    hub.deliver(&envelope(thread.clone(), event, "{\"n\":1}"));
    assert_eq!(transport.frames().len(), 1);

    // A deliberate re-delivery (for example after the client asks to resume
    // from a cursor) must not be swallowed as a duplicate.
    hub.reset_dedup(id);
    let report = hub.deliver(&envelope(thread, event, "{\"n\":1}"));
    assert_eq!(report.delivered, 1);
    assert_eq!(transport.frames().len(), 2);
}

#[test]
fn global_scope_reaches_every_subscriber() {
    let mut hub = Hub::new();
    let global = Scope::Global;

    let a = RecordingTransport::new();
    let b = RecordingTransport::new();
    hub.connect(Box::new(a.clone()), global.clone());
    hub.connect(Box::new(b.clone()), global.clone());

    let report = hub.deliver(&envelope(global, EventId::new(), "{\"global\":true}"));
    assert_eq!(report.delivered, 2);
    assert_eq!(a.frames().len(), 1);
    assert_eq!(b.frames().len(), 1);
}

#[test]
fn transport_access_is_exposed_for_specific_connections() {
    let mut hub = Hub::new();
    let thread = Scope::Thread("thr_1".into());
    let transport = RecordingTransport::new();
    let id = hub.connect(Box::new(transport.clone()), thread);

    let closed = hub.with_transport_mut(id, |sink| sink.send(b"direct"));
    assert_eq!(closed, Some(true));
    assert_eq!(transport.frames(), vec!["direct".to_string()]);

    assert!(hub.with_transport_mut(999, |_| ()).is_none());
    assert_eq!(hub.primary_scope(id), Some(&Scope::Thread("thr_1".into())));
}
