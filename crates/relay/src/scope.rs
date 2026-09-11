//! Scope-based routing.
//!
//! A [`Scope`] is the only routing primitive producers know about. It is
//! deliberately coarse: the relay maps a scope to one of a fixed number of
//! shards, so an arbitrary number of live threads/projects/hosts compresses
//! into a constant number of readers.

use serde::{Deserialize, Serialize};

/// Number of fixed relay shards.
///
/// This is a constant, not a setting, for the same reason it is one in the
/// reference design: every node runs exactly one reader per shard, so blocked
/// reader count is `node_count * SHARD_COUNT` regardless of how many scopes
/// are active. Changing it is a wire-format change because the shard of a
/// scope is derived from it.
pub const SHARD_COUNT: u8 = 8;

/// Identifier of a relay shard, always `< SHARD_COUNT`.
pub type ShardId = u8;

/// The routing key for an event.
///
/// The variants are the rooms the product actually fans out to:
/// `Thread` for a conversation, `Host` for server-to-daemon dispatch,
/// `Client` for a single connection, `Project` and `User` for list-level
/// invalidation, and `Global` for events with no narrower room.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum Scope {
    /// Every connected client on every node.
    Global,
    /// A project and its thread list.
    Project(String),
    /// One conversation.
    Thread(String),
    /// One execution machine's daemon connection.
    Host(String),
    /// One client connection.
    Client(String),
    /// One user's clients.
    User(String),
}

impl Scope {
    /// The stable, wire-level name of this scope's kind.
    pub fn kind(&self) -> &'static str {
        match self {
            Scope::Global => "global",
            Scope::Project(_) => "project",
            Scope::Thread(_) => "thread",
            Scope::Host(_) => "host",
            Scope::Client(_) => "client",
            Scope::User(_) => "user",
        }
    }

    /// The scope's identifier. `Global` has the synthetic id `"all"` so that
    /// hashing is total.
    pub fn id(&self) -> &str {
        match self {
            Scope::Global => "all",
            Scope::Project(id)
            | Scope::Thread(id)
            | Scope::Host(id)
            | Scope::Client(id)
            | Scope::User(id) => id.as_str(),
        }
    }

    /// Whether this is the global scope.
    pub fn is_global(&self) -> bool {
        matches!(self, Scope::Global)
    }

    /// The fixed shard this scope's events are appended to.
    pub fn shard(&self) -> ShardId {
        shard_for(self.kind(), self.id())
    }
}

impl std::fmt::Display for Scope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.kind(), self.id())
    }
}

const FNV_OFFSET_BASIS: u32 = 0x811c_9dc5;
const FNV_PRIME: u32 = 0x0100_0193;

fn fnv1a32_step(mut hash: u32, bytes: &[u8]) -> u32 {
    for byte in bytes {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// Deterministically maps `(kind, id)` onto a shard.
///
/// This must stay stable forever: the same hash is computed by every node and
/// by any future non-Rust backend. It is a hand-rolled FNV-1a over
/// `kind\0id` precisely so that swapping `std::hash::DefaultHasher` (which is
/// explicitly allowed to change between releases) can never silently reshuffle
/// the relay.
pub fn shard_for(kind: &str, id: &str) -> ShardId {
    let mut hash = fnv1a32_step(FNV_OFFSET_BASIS, kind.as_bytes());
    hash = fnv1a32_step(hash, &[0]);
    hash = fnv1a32_step(hash, id.as_bytes());
    (hash % u32::from(SHARD_COUNT)) as ShardId
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn shard_is_deterministic_and_in_range() {
        for i in 0..1_000 {
            let scope = Scope::Thread(format!("thr_{i}"));
            assert_eq!(scope.shard(), scope.shard());
            assert!(scope.shard() < SHARD_COUNT);
        }
    }

    #[test]
    fn many_scopes_spread_across_shards() {
        let mut seen = HashSet::new();
        for i in 0..200 {
            seen.insert(Scope::Thread(format!("t{i}")).shard());
        }
        assert_eq!(seen.len(), usize::from(SHARD_COUNT));
    }

    #[test]
    fn kind_participates_in_the_hash() {
        // Same id, different kind: must not be forced into one shard by a
        // kind-agnostic hash.
        let mut differs = false;
        for i in 0..32 {
            let id = format!("shared_{i}");
            if Scope::Project(id.clone()).shard() != Scope::Thread(id).shard() {
                differs = true;
                break;
            }
        }
        assert!(differs, "kind must affect the shard");
    }

    #[test]
    fn global_has_a_total_id() {
        assert_eq!(Scope::Global.id(), "all");
        assert!(Scope::Global.is_global());
        assert!(!Scope::Thread("x".into()).is_global());
    }

    #[test]
    fn display_is_kind_colon_id() {
        assert_eq!(Scope::Thread("thr_1".into()).to_string(), "thread:thr_1");
        assert_eq!(Scope::Global.to_string(), "global:all");
    }
}
