//! Rooms, subscriptions and idempotent fan-out.
//!
//! A [`Hub`] is the per-node connection registry. It is intentionally
//! synchronous and transport-agnostic: the server drives it from whatever
//! runtime it uses, and tests drive it directly.

use std::collections::{HashMap, HashSet};

use loom_relay::dedup::{SeenSet, DEFAULT_DEDUP_CAPACITY};
use loom_relay::envelope::Envelope;
use loom_relay::scope::Scope;

use crate::transport::Transport;

/// Identifies one connection within a hub.
pub type ConnId = u64;

/// What happened to a [`Hub::subscribe`] call.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SubscribeOutcome {
    /// The connection was not previously subscribed to this scope.
    pub newly_added: bool,
    /// The scope had no subscribers before this call. This is the edge a
    /// demand-driven backend uses to start reading, so it is reported
    /// explicitly rather than recomputed by the caller.
    pub first_subscriber: bool,
}

/// What happened during one [`Hub::deliver`] call.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DeliveryReport {
    /// Frames accepted by a transport.
    pub delivered: usize,
    /// Frames skipped because the connection had already seen that event id.
    pub duplicates: usize,
    /// Frames rejected by a transport (closed or too far behind).
    pub dropped: usize,
}

impl DeliveryReport {
    /// Connections the delivery attempted to reach.
    pub fn considered(&self) -> usize {
        self.delivered + self.duplicates + self.dropped
    }
}

struct Connection {
    transport: Box<dyn Transport>,
    primary: Scope,
    subscriptions: HashSet<Scope>,
    seen: SeenSet,
}

/// Per-node registry of connections and their scope subscriptions.
pub struct Hub {
    next_conn_id: ConnId,
    connections: HashMap<ConnId, Connection>,
    rooms: HashMap<Scope, HashSet<ConnId>>,
}

impl Hub {
    /// Creates an empty hub.
    pub fn new() -> Self {
        Self {
            next_conn_id: 0,
            connections: HashMap::new(),
            rooms: HashMap::new(),
        }
    }

    /// Registers a connection, subscribed to `primary`.
    ///
    /// A client always starts in its own scope; workers start in their host
    /// scope. Returns the new connection id.
    pub fn connect(&mut self, transport: Box<dyn Transport>, primary: Scope) -> ConnId {
        self.next_conn_id += 1;
        let id = self.next_conn_id;
        let mut subscriptions = HashSet::new();
        subscriptions.insert(primary.clone());
        self.rooms.entry(primary.clone()).or_default().insert(id);
        self.connections.insert(
            id,
            Connection {
                transport,
                primary,
                subscriptions,
                seen: SeenSet::new(DEFAULT_DEDUP_CAPACITY),
            },
        );
        id
    }

    /// Removes a connection and returns the scopes that became empty.
    ///
    /// A demand-driven backend uses the returned scopes to stop reading; a
    /// fixed-shard backend can ignore them.
    pub fn disconnect(&mut self, id: ConnId) -> Vec<Scope> {
        let Some(connection) = self.connections.remove(&id) else {
            return Vec::new();
        };
        let mut emptied = Vec::new();
        for scope in connection.subscriptions {
            if let Some(room) = self.rooms.get_mut(&scope) {
                room.remove(&id);
                if room.is_empty() {
                    self.rooms.remove(&scope);
                    emptied.push(scope);
                }
            }
        }
        emptied
    }

    /// Subscribes an existing connection to an additional scope.
    pub fn subscribe(&mut self, id: ConnId, scope: Scope) -> SubscribeOutcome {
        let Some(connection) = self.connections.get_mut(&id) else {
            return SubscribeOutcome::default();
        };
        if !connection.subscriptions.insert(scope.clone()) {
            return SubscribeOutcome::default();
        }
        let room = self.rooms.entry(scope).or_default();
        let first_subscriber = room.is_empty();
        room.insert(id);
        SubscribeOutcome {
            newly_added: true,
            first_subscriber,
        }
    }

    /// Removes a subscription. Returns `true` if it had been subscribed.
    ///
    /// A connection's primary scope cannot be removed; disconnect instead.
    pub fn unsubscribe(&mut self, id: ConnId, scope: &Scope) -> bool {
        let Some(connection) = self.connections.get_mut(&id) else {
            return false;
        };
        if &connection.primary == scope {
            return false;
        }
        if !connection.subscriptions.remove(scope) {
            return false;
        }
        if let Some(room) = self.rooms.get_mut(scope) {
            room.remove(&id);
            if room.is_empty() {
                self.rooms.remove(scope);
            }
        }
        true
    }

    /// Delivers an envelope to every subscriber of its scope.
    ///
    /// Each connection receives the frame at most once per `event_id`, so a
    /// frame that arrives both locally and by replay is not shown twice.
    pub fn deliver(&mut self, envelope: &Envelope) -> DeliveryReport {
        let mut report = DeliveryReport::default();
        let Some(recipients) = self.rooms.get(&envelope.scope) else {
            return report;
        };
        // Collect first so the connections map can be borrowed mutably below.
        let recipients: Vec<ConnId> = recipients.iter().copied().collect();

        for id in recipients {
            let Some(connection) = self.connections.get_mut(&id) else {
                continue;
            };
            if !connection.seen.insert(envelope.event_id) {
                report.duplicates += 1;
                continue;
            }
            if connection.transport.send(&envelope.payload) {
                report.delivered += 1;
            } else {
                report.dropped += 1;
            }
        }
        report
    }

    /// Delivers many envelopes in order.
    pub fn deliver_all<'a, I>(&mut self, envelopes: I) -> DeliveryReport
    where
        I: IntoIterator<Item = &'a Envelope>,
    {
        let mut total = DeliveryReport::default();
        for envelope in envelopes {
            let report = self.deliver(envelope);
            total.delivered += report.delivered;
            total.duplicates += report.duplicates;
            total.dropped += report.dropped;
        }
        total
    }

    /// Number of connections subscribed to a scope.
    pub fn subscriber_count(&self, scope: &Scope) -> usize {
        self.rooms.get(scope).map(HashSet::len).unwrap_or(0)
    }

    /// Number of live connections.
    pub fn connection_count(&self) -> usize {
        self.connections.len()
    }

    /// Number of non-empty rooms.
    pub fn room_count(&self) -> usize {
        self.rooms.len()
    }

    /// Scopes with at least one subscriber.
    pub fn active_scopes(&self) -> Vec<Scope> {
        self.rooms.keys().cloned().collect()
    }

    /// Whether a connection is currently registered.
    pub fn is_connected(&self, id: ConnId) -> bool {
        self.connections.contains_key(&id)
    }

    /// The scope a connection is anchored to.
    pub fn primary_scope(&self, id: ConnId) -> Option<&Scope> {
        self.connections.get(&id).map(|c| &c.primary)
    }

    /// Accesses a connection's transport. Used by callers that need to close
    /// or reconfigure a specific socket.
    pub fn with_transport_mut<R>(
        &mut self,
        id: ConnId,
        visit: impl FnOnce(&mut dyn Transport) -> R,
    ) -> Option<R> {
        self.connections
            .get_mut(&id)
            .map(|connection| visit(connection.transport.as_mut()))
    }

    /// Forgets the dedup history of one connection.
    ///
    /// Called after a replay is prepared, so the replayed frames are treated as
    /// new rather than suppressed as duplicates.
    pub fn reset_dedup(&mut self, id: ConnId) {
        if let Some(connection) = self.connections.get_mut(&id) {
            connection.seen = SeenSet::new(connection.seen.capacity());
        }
    }
}

impl Default for Hub {
    fn default() -> Self {
        Hub::new()
    }
}

impl std::fmt::Debug for Hub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Hub")
            .field("connections", &self.connections.len())
            .field("rooms", &self.rooms.len())
            .finish_non_exhaustive()
    }
}
