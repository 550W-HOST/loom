//! The hub actor.
//!
//! [`Hub`](loom_relay_hub::Hub) is synchronous: it owns the connection table
//! and answers "who gets this frame". Rather than put a lock around it and let
//! every request handler contend, one task owns it and everyone else sends
//! messages. The consequences are worth the indirection:
//!
//! * delivery order is exactly the order the actor processed envelopes, so two
//!   subscribers cannot observe contradictory orderings;
//! * the connection table is never shared, so there is no lock to hold across a
//!   send;
//! * a slow client is dropped by its own transport, not by blocking the actor.
//!
//! `Deliver` is deliberately fire-and-forget. It is the hot path, and a
//! publisher must not be able to block on a subscriber.

use loom_relay::envelope::Envelope;
use loom_relay::scope::Scope;
use loom_relay_hub::{ConnId, DeliveryReport, Hub, SubscribeOutcome, Transport};
use tokio::sync::{mpsc, oneshot};

/// Commands accepted by the hub actor.
///
/// Deliberately not `Debug`: a variant holds a `Box<dyn Transport>`, and
/// formatting a connection's sink is never useful. The manual impl below
/// reports variant names only.
pub enum HubCommand {
    /// Register a connection subscribed to `scope`.
    Connect {
        /// The connection's sink.
        transport: Box<dyn Transport>,
        /// The scope the connection starts subscribed to.
        scope: Scope,
        /// Receives the new connection id.
        reply: oneshot::Sender<ConnId>,
    },
    /// Add a subscription to an existing connection.
    Subscribe {
        /// Target connection.
        conn: ConnId,
        /// Scope to add.
        scope: Scope,
        /// Receives whether the scope gained its first subscriber.
        reply: oneshot::Sender<SubscribeOutcome>,
    },
    /// Remove a connection. Empty rooms are cleaned up.
    Disconnect {
        /// Connection to remove.
        conn: ConnId,
    },
    /// Remove a single subscription from a connection.
    Unsubscribe {
        /// Target connection.
        conn: ConnId,
        /// Scope to leave.
        scope: Scope,
        /// Receives whether the subscription existed.
        reply: oneshot::Sender<bool>,
    },
    /// Queue a frame directly to one connection.
    ///
    /// Used for control replies. It goes through the same queue as relayed
    /// events, so an acknowledgement is never reordered ahead of the events it
    /// enables.
    SendTo {
        /// Target connection.
        conn: ConnId,
        /// The frame to queue.
        frame: bytes::Bytes,
        /// Receives whether the transport accepted it.
        reply: oneshot::Sender<bool>,
    },
    /// Deliver an envelope to every local subscriber. Hot path.
    Deliver {
        /// The envelope to fan out.
        envelope: Box<Envelope>,
    },
}

/// A cheap, cloneable handle to the hub actor.
#[derive(Clone, Debug)]
pub struct HubHandle {
    sender: mpsc::Sender<HubCommand>,
}

/// Raised when the hub actor is gone and a command cannot be answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HubUnavailable;

impl std::fmt::Display for HubUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("hub actor is no longer running")
    }
}

impl std::error::Error for HubUnavailable {}

impl HubHandle {
    /// Spawns the actor, returning a handle and the task's join handle.
    ///
    /// `capacity` bounds how many commands may be queued before publishers
    /// block. Delivery is fire-and-forget, so this only fills up if commands
    /// (connects, subscribes) arrive faster than they are processed.
    pub fn spawn(capacity: usize) -> (Self, tokio::task::JoinHandle<()>) {
        let (sender, receiver) = mpsc::channel(capacity.max(1));
        let task = tokio::spawn(run_hub(Hub::new(), receiver));
        (Self { sender }, task)
    }

    /// Spawns the actor around an existing hub.
    pub fn spawn_with_hub(hub: Hub, capacity: usize) -> (Self, tokio::task::JoinHandle<()>) {
        let (sender, receiver) = mpsc::channel(capacity.max(1));
        let task = tokio::spawn(run_hub(hub, receiver));
        (Self { sender }, task)
    }

    /// Registers a connection.
    pub async fn connect(
        &self,
        transport: Box<dyn Transport>,
        scope: Scope,
    ) -> Result<ConnId, HubUnavailable> {
        let (reply, answer) = oneshot::channel();
        self.sender
            .send(HubCommand::Connect {
                transport,
                scope,
                reply,
            })
            .await
            .map_err(|_| HubUnavailable)?;
        answer.await.map_err(|_| HubUnavailable)
    }

    /// Adds a subscription to an existing connection.
    pub async fn subscribe(
        &self,
        conn: ConnId,
        scope: Scope,
    ) -> Result<SubscribeOutcome, HubUnavailable> {
        let (reply, answer) = oneshot::channel();
        self.sender
            .send(HubCommand::Subscribe { conn, scope, reply })
            .await
            .map_err(|_| HubUnavailable)?;
        answer.await.map_err(|_| HubUnavailable)
    }

    /// Removes a connection.
    pub async fn disconnect(&self, conn: ConnId) -> Result<(), HubUnavailable> {
        self.sender
            .send(HubCommand::Disconnect { conn })
            .await
            .map_err(|_| HubUnavailable)
    }

    /// Removes a single subscription from a connection.
    pub async fn unsubscribe(&self, conn: ConnId, scope: Scope) -> Result<bool, HubUnavailable> {
        let (reply, answer) = oneshot::channel();
        self.sender
            .send(HubCommand::Unsubscribe { conn, scope, reply })
            .await
            .map_err(|_| HubUnavailable)?;
        answer.await.map_err(|_| HubUnavailable)
    }

    /// Queues a frame directly to one connection, awaiting the outcome.
    pub async fn send_to(
        &self,
        conn: ConnId,
        frame: impl Into<bytes::Bytes>,
    ) -> Result<bool, HubUnavailable> {
        let (reply, answer) = oneshot::channel();
        self.sender
            .send(HubCommand::SendTo {
                conn,
                frame: frame.into(),
                reply,
            })
            .await
            .map_err(|_| HubUnavailable)?;
        answer.await.map_err(|_| HubUnavailable)
    }

    /// Delivers an envelope, without waiting for the fan-out to happen.
    ///
    /// Returns `Err(HubUnavailable)` only when the actor has stopped; a full
    /// command queue applies backpressure to the caller, which is the intended
    /// behaviour for a producer that is outrunning the fan-out.
    pub async fn deliver(&self, envelope: Envelope) -> Result<(), HubUnavailable> {
        self.sender
            .send(HubCommand::Deliver {
                envelope: Box::new(envelope),
            })
            .await
            .map_err(|_| HubUnavailable)
    }

    /// Delivers an envelope without applying backpressure.
    ///
    /// Used by the relay pump, which must never stall the log reader. A full
    /// queue means fan-out is behind; the frame is dropped and the subscriber
    /// recovers by replaying from its last seen event id.
    pub fn try_deliver(&self, envelope: Envelope) -> DeliveryOutcome {
        match self.sender.try_send(HubCommand::Deliver {
            envelope: Box::new(envelope),
        }) {
            Ok(()) => DeliveryOutcome::Queued,
            Err(mpsc::error::TrySendError::Full(_)) => DeliveryOutcome::Backpressured,
            Err(mpsc::error::TrySendError::Closed(_)) => DeliveryOutcome::Stopped,
        }
    }

    /// Whether the actor is still running.
    pub fn is_running(&self) -> bool {
        !self.sender.is_closed()
    }
}

/// Result of a non-blocking [`HubHandle::try_deliver`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeliveryOutcome {
    /// Handed to the actor.
    Queued,
    /// The actor's queue is full; the frame was dropped.
    Backpressured,
    /// The actor has stopped.
    Stopped,
}

impl std::fmt::Debug for HubCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            HubCommand::Connect { .. } => "Connect",
            HubCommand::Subscribe { .. } => "Subscribe",
            HubCommand::Disconnect { .. } => "Disconnect",
            HubCommand::Unsubscribe { .. } => "Unsubscribe",
            HubCommand::SendTo { .. } => "SendTo",
            HubCommand::Deliver { .. } => "Deliver",
        };
        f.write_str(name)
    }
}

/// The actor loop. Runs until every handle is dropped.
async fn run_hub(mut hub: Hub, mut receiver: mpsc::Receiver<HubCommand>) {
    while let Some(command) = receiver.recv().await {
        match command {
            HubCommand::Connect {
                transport,
                scope,
                reply,
            } => {
                let id = hub.connect(transport, scope);
                let _ = reply.send(id);
            }
            HubCommand::Subscribe { conn, scope, reply } => {
                let outcome = hub.subscribe(conn, scope);
                let _ = reply.send(outcome);
            }
            HubCommand::Disconnect { conn } => {
                hub.disconnect(conn);
            }
            HubCommand::Unsubscribe { conn, scope, reply } => {
                let removed = hub.unsubscribe(conn, &scope);
                let _ = reply.send(removed);
            }
            HubCommand::SendTo { conn, frame, reply } => {
                let accepted = hub
                    .with_transport_mut(conn, |sink| sink.send(&frame))
                    .unwrap_or(false);
                let _ = reply.send(accepted);
            }
            HubCommand::Deliver { envelope } => {
                let report = hub.deliver(&envelope);
                record_delivery(&report);
            }
        }
    }
}

/// Hook for metrics. Kept free-standing so the actor has no logging dependency.
fn record_delivery(_report: &DeliveryReport) {}

#[cfg(test)]
mod tests {
    use super::*;
    use loom_relay::event_id::EventId;
    use loom_relay_hub::RecordingTransport;
    use tokio::time::{timeout, Duration};

    fn envelope(scope: Scope, payload: &str) -> Envelope {
        Envelope {
            event_id: EventId::new(),
            scope,
            payload: bytes::Bytes::copy_from_slice(payload.as_bytes()),
            created_at_ms: 1,
            origin: "test".into(),
            exclude: None,
        }
    }

    async fn settle() {
        // Give the actor a chance to process queued commands.
        tokio::task::yield_now().await;
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    #[tokio::test]
    async fn delivers_to_a_connected_transport() {
        let (hub, task) = HubHandle::spawn(16);
        let transport = RecordingTransport::new();
        let scope = Scope::Thread("thr_1".into());

        hub.connect(Box::new(transport.clone()), scope.clone())
            .await
            .unwrap();
        hub.deliver(envelope(scope, "{\"n\":1}")).await.unwrap();
        settle().await;

        assert_eq!(transport.frames(), vec!["{\"n\":1}".to_string()]);
        drop(hub);
        timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn subscribe_returns_the_first_subscriber_edge() {
        let (hub, task) = HubHandle::spawn(16);
        let scope = Scope::Thread("thr_1".into());
        let other = Scope::User("user_1".into());

        let a = hub
            .connect(Box::new(RecordingTransport::new()), other.clone())
            .await
            .unwrap();
        let b = hub
            .connect(Box::new(RecordingTransport::new()), other)
            .await
            .unwrap();

        assert!(
            hub.subscribe(a, scope.clone())
                .await
                .unwrap()
                .first_subscriber
        );
        assert!(!hub.subscribe(b, scope).await.unwrap().first_subscriber);

        drop(hub);
        timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn disconnect_stops_delivery() {
        let (hub, task) = HubHandle::spawn(16);
        let transport = RecordingTransport::new();
        let scope = Scope::Thread("thr_1".into());

        let id = hub
            .connect(Box::new(transport.clone()), scope.clone())
            .await
            .unwrap();
        hub.disconnect(id).await.unwrap();
        hub.deliver(envelope(scope, "{}")).await.unwrap();
        settle().await;

        assert!(transport.is_empty());
        drop(hub);
        timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn handles_report_when_the_actor_has_stopped() {
        let (hub, task) = HubHandle::spawn(4);
        // Aborting drops the actor future, which drops its receiver. Dropping
        // the join handle alone would only detach the task.
        task.abort();
        let _ = task.await;

        assert!(hub
            .connect(
                Box::new(RecordingTransport::new()),
                Scope::Thread("t".into())
            )
            .await
            .is_err());
        assert!(!hub.is_running());
    }
}
