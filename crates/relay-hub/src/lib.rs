//! The connection and fan-out half of the relay.
//!
//! [`loom_relay`] answers *"does this event reach this node?"*.
//! This crate answers *"which sockets on this node get it, exactly once?"*.
//!
//! The split exists so that neither concern constrains the other:
//!
//! * the relay can be tested, measured and re-backed (memory -> disk)
//!   without opening a socket;
//! * the hub can be tested with an in-memory transport, without a broker.
//!
//! A [`Hub`] owns rooms keyed by [`Scope`](loom_relay::Scope) and a set of
//! connections. Delivery is filtered twice: by subscription (does this
//! connection want this scope?) and by identity (has this connection already
//! seen this [`EventId`](loom_relay::EventId)?). The second filter is what makes
//! replay safe — the same frame arriving twice is delivered once.

#![forbid(unsafe_code)]

pub mod hub;
pub mod transport;

pub use hub::{ConnId, DeliveryReport, Hub, SubscribeOutcome};
pub use transport::{RecordingTransport, SharedTransport, Transport};
