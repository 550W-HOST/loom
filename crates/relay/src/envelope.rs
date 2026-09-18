//! The unit that crosses process and node boundaries.
//!
//! Producers build an [`Envelope`]; backends persist and forward it; connection
//! layers read it. The only thing the rest of the system depends on is the
//! [`Scope`] and the [`EventId`] — never on a socket.

use bytes::Bytes;
use serde::{Deserialize, Serialize};

use crate::error::{RelayError, Result};
use crate::event_id::EventId;
use crate::scope::Scope;

/// Identifies the node that produced an envelope.
pub type NodeId = String;

/// One relayed event, as seen in-process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Envelope {
    /// Monotonic identity, used for ordering, replay cursors and dedup.
    pub event_id: EventId,
    /// The room this event belongs to.
    pub scope: Scope,
    /// The frame exactly as it should reach a client or worker.
    pub payload: Bytes,
    /// Producer wall-clock time, in milliseconds since the epoch.
    pub created_at_ms: u64,
    /// The node that produced this event.
    pub origin: NodeId,
    /// When set, the producing node already delivered this locally and the
    /// named origin must not deliver it a second time.
    pub exclude: Option<NodeId>,
}

impl Envelope {
    /// Builds a new envelope with a freshly minted [`EventId`].
    pub fn new(origin: impl Into<NodeId>, scope: Scope, payload: impl Into<Bytes>) -> Self {
        Self {
            event_id: EventId::new(),
            scope,
            payload: payload.into(),
            created_at_ms: crate::now_ms(),
            origin: origin.into(),
            exclude: None,
        }
    }

    /// Builds an envelope with an explicit timestamp (import, backfill, tests).
    pub fn with_created_at(
        origin: impl Into<NodeId>,
        scope: Scope,
        payload: impl Into<Bytes>,
        created_at_ms: u64,
    ) -> Self {
        Self {
            event_id: EventId::new(),
            scope,
            payload: payload.into(),
            created_at_ms,
            origin: origin.into(),
            exclude: None,
        }
    }

    /// The serializable cross-process form.
    pub fn to_wire(&self) -> WireEnvelope {
        WireEnvelope {
            event_id: self.event_id.to_string(),
            scope: self.scope.clone(),
            payload: String::from_utf8_lossy(&self.payload).into_owned(),
            created_at_ms: self.created_at_ms,
            origin: self.origin.clone(),
            exclude: self.exclude.clone(),
        }
    }
}

impl From<&Envelope> for WireEnvelope {
    fn from(envelope: &Envelope) -> Self {
        envelope.to_wire()
    }
}

/// The wire form of an [`Envelope`].
///
/// `payload` is text because the frames travelling over the relay are already
/// JSON; keeping it a string means a backend never has to decide how to carry
/// bytes, and an operator can read the log with `jq`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireEnvelope {
    /// Text form of the [`EventId`].
    pub event_id: String,
    /// The room this event belongs to.
    pub scope: Scope,
    /// The frame, as UTF-8 text.
    pub payload: String,
    /// Producer wall-clock time in milliseconds.
    pub created_at_ms: u64,
    /// Producing node.
    pub origin: String,
    /// Node that must skip local delivery.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude: Option<String>,
}

impl TryFrom<WireEnvelope> for Envelope {
    type Error = RelayError;

    fn try_from(wire: WireEnvelope) -> Result<Self> {
        let event_id = wire
            .event_id
            .parse::<EventId>()
            .map_err(|error| RelayError::InvalidEventId(error.to_string()))?;
        Ok(Envelope {
            event_id,
            scope: wire.scope,
            payload: Bytes::from(wire.payload.into_bytes()),
            created_at_ms: wire.created_at_ms,
            origin: wire.origin,
            exclude: wire.exclude,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_round_trip_preserves_every_field() {
        let mut envelope = Envelope::new("node-a", Scope::Thread("thr_1".into()), "{\"n\":1}");
        envelope.exclude = Some("node-b".into());

        let wire = envelope.to_wire();
        let restored = Envelope::try_from(wire).unwrap();

        assert_eq!(restored, envelope);
    }

    #[test]
    fn wire_form_is_json_and_readable() {
        let envelope = Envelope::new("node-a", Scope::Host("host_1".into()), "{\"n\":2}");
        let json = serde_json::to_value(envelope.to_wire()).unwrap();

        assert_eq!(json["scope"]["kind"], "host");
        assert_eq!(json["scope"]["id"], "host_1");
        assert_eq!(json["payload"], "{\"n\":2}");
        assert_eq!(json["event_id"].as_str().unwrap().len(), 26);
    }

    #[test]
    fn wire_round_trips_through_json() {
        let envelope = Envelope::new("node-a", Scope::Project("proj_1".into()), "{}");
        let text = serde_json::to_string(&envelope.to_wire()).unwrap();
        let decoded: WireEnvelope = serde_json::from_str(&text).unwrap();
        assert_eq!(Envelope::try_from(decoded).unwrap(), envelope);
    }

    #[test]
    fn invalid_event_id_is_rejected() {
        let wire = WireEnvelope {
            event_id: "not-an-event-id".into(),
            scope: Scope::Global,
            payload: "{}".into(),
            created_at_ms: 0,
            origin: "node-a".into(),
            exclude: None,
        };
        assert!(matches!(
            Envelope::try_from(wire),
            Err(RelayError::InvalidEventId(_))
        ));
    }

    #[test]
    fn created_at_is_honoured_for_backfill() {
        let envelope = Envelope::with_created_at("node-a", Scope::Global, "{}", 123);
        assert_eq!(envelope.created_at_ms, 123);
    }
}
