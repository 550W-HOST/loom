//! Hosts: the identity a daemon registers.
//!
//! A host is a *machine*, not a connection. Its status tracks whether a daemon
//! is currently attached; the id survives reconnects. There is deliberately no
//! provider or plugin registration on a host — providers are first-class
//! elsewhere, and hosts only say "this machine can run work".

use serde::{Deserialize, Serialize};

use crate::error::DomainError;
use crate::event::DomainEvent;
use crate::id::HostId;

/// The kind of machine a host identifies.
///
/// Only one kind exists today. It is an enum rather than a bool so a future
/// ephemeral runner is an added variant, not a reinterpreted field.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostKind {
    /// A long-lived machine that daemons enroll.
    #[default]
    Persistent,
}

/// Whether a daemon is currently attached to this host.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostStatus {
    /// A daemon holds an active connection.
    Connected,
    /// No daemon is attached; the host still exists.
    Disconnected,
}

/// The identity of a machine that runs work.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Host {
    /// Identity, stable across reconnects.
    pub id: HostId,
    /// Display name, chosen by the operator or the machine.
    pub name: String,
    /// Machine kind.
    pub kind: HostKind,
    /// Whether a daemon is attached.
    pub status: HostStatus,
    /// Wall-clock milliseconds of the last heartbeat, if ever seen.
    pub last_seen_at_ms: Option<u64>,
    /// Wall-clock milliseconds when the host was first registered.
    pub created_at_ms: u64,
    /// Wall-clock milliseconds of the last mutation.
    pub updated_at_ms: u64,
}

impl Host {
    /// Registers a host and returns the event it produces.
    ///
    /// Registration happens when a daemon first announces itself, so the
    /// initial status is `connected` and `last_seen_at_ms` is set.
    pub fn register(
        name: impl Into<String>,
        now_ms: u64,
    ) -> Result<(Self, DomainEvent), DomainError> {
        let name = name.into().trim().to_owned();
        if name.is_empty() {
            return Err(DomainError::InvalidField {
                field: "name",
                reason: "must not be empty".into(),
            });
        }
        let host = Self {
            id: HostId::mint(),
            name,
            kind: HostKind::Persistent,
            status: HostStatus::Connected,
            last_seen_at_ms: Some(now_ms),
            created_at_ms: now_ms,
            updated_at_ms: now_ms,
        };
        let event = DomainEvent::HostRegistered { host: host.clone() };
        Ok((host, event))
    }

    /// Records a heartbeat without emitting an event; heartbeats are high
    /// frequency and not worth a frame.
    pub fn heartbeat(&mut self, now_ms: u64) {
        self.last_seen_at_ms = Some(now_ms);
        self.updated_at_ms = now_ms;
    }

    /// Marks the daemon attached, returning an event on an actual change.
    pub fn mark_connected(&mut self, now_ms: u64) -> Option<DomainEvent> {
        self.last_seen_at_ms = Some(now_ms);
        self.transition(HostStatus::Connected, now_ms)
    }

    /// Marks the daemon detached, returning an event on an actual change.
    pub fn mark_disconnected(&mut self, now_ms: u64) -> Option<DomainEvent> {
        self.transition(HostStatus::Disconnected, now_ms)
    }

    fn transition(&mut self, to: HostStatus, now_ms: u64) -> Option<DomainEvent> {
        if self.status == to {
            self.updated_at_ms = now_ms;
            return None;
        }
        let from = self.status;
        self.status = to;
        self.updated_at_ms = now_ms;
        Some(DomainEvent::HostStatusChanged {
            host_id: self.id.clone(),
            from,
            to,
            at_ms: now_ms,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_rejects_an_empty_name() {
        assert!(matches!(
            Host::register("   ", 1),
            Err(DomainError::InvalidField { field: "name", .. })
        ));
    }

    #[test]
    fn register_starts_connected() {
        let (host, event) = Host::register("laptop", 5).unwrap();
        assert_eq!(host.status, HostStatus::Connected);
        assert_eq!(host.last_seen_at_ms, Some(5));
        assert!(matches!(event, DomainEvent::HostRegistered { .. }));
    }

    #[test]
    fn status_changes_only_emit_on_an_actual_change() {
        let (mut host, _) = Host::register("laptop", 5).unwrap();
        assert!(host.mark_connected(6).is_none());

        let event = host.mark_disconnected(7).unwrap();
        match event {
            DomainEvent::HostStatusChanged { from, to, .. } => {
                assert_eq!(from, HostStatus::Connected);
                assert_eq!(to, HostStatus::Disconnected);
            }
            other => panic!("expected a status change, got {other:?}"),
        }
        assert!(host.mark_disconnected(8).is_none());

        // A heartbeat still updates the timestamp without an event.
        host.heartbeat(9);
        assert_eq!(host.last_seen_at_ms, Some(9));
        assert_eq!(host.updated_at_ms, 9);
    }
}
