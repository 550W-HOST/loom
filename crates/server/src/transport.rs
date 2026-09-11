//! A [`Transport`](loom_relay_hub::Transport) backed by a bounded channel.
//!
//! This is where backpressure becomes a real, observable policy rather than a
//! comment. The hub asks the transport to send; the transport does a
//! non-blocking `try_send` into a bounded queue owned by the connection's write
//! task. Three outcomes:
//!
//! * queued — the frame is on its way;
//! * full — the socket is not draining, so the frame is dropped and the hub is
//!   told so, instead of the server buffering without bound;
//! * closed — the write task is gone, so the connection is effectively dead.
//!
//! A dropped frame is not lost data. The client recovers by resuming from its
//! last delivered event id, which is exactly what the relay's replay window is
//! for.

use bytes::Bytes;
use loom_relay_hub::Transport;
use tokio::sync::mpsc;

/// Default per-connection outbound queue depth.
pub const DEFAULT_OUTBOUND_CAPACITY: usize = 256;

/// Forwards frames into a bounded queue drained by the connection's writer.
#[derive(Clone, Debug)]
pub struct ChannelTransport {
    sender: mpsc::Sender<Bytes>,
}

impl ChannelTransport {
    /// Builds a transport and the receiving half of its queue.
    pub fn new(capacity: usize) -> (Self, mpsc::Receiver<Bytes>) {
        let (sender, receiver) = mpsc::channel(capacity.max(1));
        (Self { sender }, receiver)
    }

    /// Builds a transport with the default queue depth.
    pub fn with_default_capacity() -> (Self, mpsc::Receiver<Bytes>) {
        Self::new(DEFAULT_OUTBOUND_CAPACITY)
    }

    /// Number of free slots, for metrics and tests.
    pub fn capacity(&self) -> usize {
        self.sender.capacity()
    }

    /// Whether the writer has gone away.
    pub fn is_closed(&self) -> bool {
        self.sender.is_closed()
    }
}

impl Transport for ChannelTransport {
    fn send(&mut self, frame: &[u8]) -> bool {
        // `try_send` never blocks: an event loop must not stall because one
        // client stopped reading.
        self.sender.try_send(Bytes::copy_from_slice(frame)).is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn queued_frames_arrive_in_order() {
        let (mut transport, mut receiver) = ChannelTransport::new(8);
        assert!(transport.send(b"a"));
        assert!(transport.send(b"b"));

        assert_eq!(receiver.recv().await.unwrap(), Bytes::from_static(b"a"));
        assert_eq!(receiver.recv().await.unwrap(), Bytes::from_static(b"b"));
    }

    #[tokio::test]
    async fn a_full_queue_drops_instead_of_blocking() {
        let (mut transport, _receiver) = ChannelTransport::new(2);
        assert!(transport.send(b"1"));
        assert!(transport.send(b"2"));
        // Nothing has drained the queue.
        assert!(!transport.send(b"3"), "must report backpressure, not block");
        assert!(!transport.send(b"4"));
    }

    #[tokio::test]
    async fn a_closed_receiver_is_reported_as_a_failure() {
        let (mut transport, receiver) = ChannelTransport::new(4);
        drop(receiver);
        assert!(transport.is_closed());
        assert!(!transport.send(b"x"));
    }

    #[tokio::test]
    async fn capacity_is_never_zero() {
        let (transport, _receiver) = ChannelTransport::new(0);
        assert!(transport.capacity() >= 1);
    }
}
