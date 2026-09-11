//! Where a delivered frame goes.
//!
//! A [`Transport`] is the hub's view of one connection. In production it is a
//! WebSocket sink with a bounded queue; [`send`](Transport::send) returning
//! `false` is the backpressure signal that lets the hub stay responsive when a
//! client stops reading, instead of growing an unbounded buffer per connection.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// A sink for frames delivered to one connection.
pub trait Transport: Send {
    /// Queues one frame.
    ///
    /// Returns `true` when the frame was accepted, `false` when the connection
    /// is closed or too far behind. The hub counts a `false` as a drop and
    /// does not retry — a client that cannot keep up recovers by replaying
    /// from its last seen [`EventId`](loom_relay::EventId).
    fn send(&mut self, frame: &[u8]) -> bool;
}

/// Records accepted frames and can be observed from outside the hub.
#[derive(Debug, Default)]
struct RecordingState {
    frames: Mutex<Vec<String>>,
    closed: AtomicBool,
    sends: AtomicUsize,
}

/// An in-memory [`Transport`] whose frames can be inspected after delivery.
///
/// Cheap to clone: every clone observes the same connection.
#[derive(Clone, Debug, Default)]
pub struct RecordingTransport {
    state: Arc<RecordingState>,
}

impl RecordingTransport {
    /// Creates an open recording transport.
    pub fn new() -> Self {
        Self::default()
    }

    /// Frames accepted so far, in delivery order.
    pub fn frames(&self) -> Vec<String> {
        self.state
            .frames
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
    }

    /// Number of frames accepted so far.
    pub fn len(&self) -> usize {
        self.state
            .frames
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .len()
    }

    /// Whether nothing has been accepted.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Total `send` calls, including rejected ones.
    pub fn send_attempts(&self) -> usize {
        self.state.sends.load(Ordering::SeqCst)
    }

    /// Number of accepted frames.
    pub fn accepted(&self) -> usize {
        self.state
            .frames
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .len()
    }

    /// Closes the connection; later sends are rejected.
    pub fn close(&self) {
        self.state.closed.store(true, Ordering::SeqCst);
    }

    /// Whether the connection is closed.
    pub fn is_closed(&self) -> bool {
        self.state.closed.load(Ordering::SeqCst)
    }
}

impl Transport for RecordingTransport {
    fn send(&mut self, frame: &[u8]) -> bool {
        self.state.sends.fetch_add(1, Ordering::SeqCst);
        if self.state.closed.load(Ordering::SeqCst) {
            return false;
        }
        self.state
            .frames
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .push(String::from_utf8_lossy(frame).into_owned());
        true
    }
}

/// A [`Transport`] shared across the hub boundary.
///
/// The hub stores `Box<dyn Transport>`; this is the usual thing to put in it
/// when the caller also wants to observe or close the connection.
pub type SharedTransport = RecordingTransport;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_frames_in_order() {
        let transport = RecordingTransport::new();
        let mut sink = transport.clone();
        assert!(sink.send(b"a"));
        assert!(sink.send(b"b"));
        assert_eq!(transport.frames(), vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn closed_transport_rejects_without_recording() {
        let transport = RecordingTransport::new();
        let mut sink = transport.clone();
        assert!(sink.send(b"a"));
        transport.close();
        assert!(!sink.send(b"b"));
        assert_eq!(transport.frames(), vec!["a".to_string()]);
        assert_eq!(transport.send_attempts(), 2);
        assert_eq!(transport.accepted(), 1);
    }
}
