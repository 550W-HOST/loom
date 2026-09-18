//! Scope mapping.
//!
//! The relay routes by a `(kind, id)` pair. The domain names those pairs with
//! [`DomainScope`], so producers never hand the relay a bare string. The server
//! converts a `DomainScope` into a relay scope at the publish boundary
//! (`loom_server::state`), which is the only place that knows both crates.
//!
//! ```text
//! thread:{id}   one conversation — thread messages and status changes
//! project:{id}  a project's list-level state — its threads, environments
//! host:{id}     one execution machine — registration and dispatch
//! user:{id}     one user's clients (reserved; no user entity yet)
//! global        no narrower room (a new project, server-wide notices)
//! ```
//!
//! [`DomainEvent::scope`](crate::DomainEvent::scope) assigns exactly one scope
//! per event. One event, one scope is deliberate: a client subscribes to the
//! two scopes it is displaying, and receiving the same fact under two ids
//! would be a duplicate it cannot deduplicate.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::id::{HostId, ProjectId, ThreadId, UserId};

/// The room a domain event belongs to.
///
/// Serializes as the relay's `(kind, id)` shape, so a `DomainScope` and a
/// `loom_relay::Scope` describe the same room on the wire:
///
/// ```json
/// {"kind":"thread","id":"thr_…"}
/// {"kind":"global"}
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum DomainScope {
    /// Every connected client, on every node.
    Global,
    /// A project and its list-level state.
    Project(ProjectId),
    /// One conversation.
    Thread(ThreadId),
    /// One execution machine's worker room.
    Host(HostId),
    /// One user's clients. Reserved: no user entity exists yet.
    User(UserId),
}

impl DomainScope {
    /// The stable, wire-level kind name.
    pub fn kind(&self) -> &'static str {
        match self {
            DomainScope::Global => "global",
            DomainScope::Project(_) => "project",
            DomainScope::Thread(_) => "thread",
            DomainScope::Host(_) => "host",
            DomainScope::User(_) => "user",
        }
    }

    /// The scope's identifier, or `None` for [`DomainScope::Global`].
    pub fn id(&self) -> Option<&str> {
        match self {
            DomainScope::Global => None,
            DomainScope::Project(id) => Some(id.as_str()),
            DomainScope::Thread(id) => Some(id.as_str()),
            DomainScope::Host(id) => Some(id.as_str()),
            DomainScope::User(id) => Some(id.as_str()),
        }
    }

    /// Whether this is the global scope.
    pub fn is_global(&self) -> bool {
        matches!(self, DomainScope::Global)
    }
}

impl fmt::Display for DomainScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DomainScope::Global => f.write_str("global"),
            other => write!(f, "{}:{}", other.kind(), other.id().unwrap_or_default()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_kind_colon_id() {
        let thread = ThreadId::mint();
        assert_eq!(
            DomainScope::Thread(thread.clone()).to_string(),
            format!("thread:{thread}")
        );
        assert_eq!(DomainScope::Global.to_string(), "global");
    }

    #[test]
    fn serde_matches_the_relay_shape() {
        assert_eq!(
            serde_json::to_value(DomainScope::Global).unwrap(),
            serde_json::json!({ "kind": "global" })
        );
        let host = HostId::mint();
        assert_eq!(
            serde_json::to_value(DomainScope::Host(host.clone())).unwrap(),
            serde_json::json!({ "kind": "host", "id": host.to_string() })
        );
    }
}
