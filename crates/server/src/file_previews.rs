//! Short-lived, host-bound file preview leases.
//!
//! A preview URL is a capability returned by the `files.createPreview` route.
//! The URL must never turn a client-controlled host id and absolute path into
//! an arbitrary filesystem read, so the lease binds the request to one host and
//! one root. The B8 content route only accepts a lease token.
//!
//! Leases are deliberately process-local capabilities. They are not part of
//! the stored entity view or the relay log: a restart invalidates them, and a
//! request routed to another node cannot use one. Deployments that need
//! cross-node preview URLs must add a shared capability store and routing
//! affinity; the in-memory registry remains the zero-dependency default.

use std::collections::HashMap;
use std::sync::Mutex;

use loom_domain::HostId;
use loom_relay::{now_ms, EventId};

/// Default lifetime for a preview when the caller omits `ttlMs`.
pub const DEFAULT_FILE_PREVIEW_TTL_MS: u64 = 5 * 60 * 1_000;

/// A root-bound preview capability.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FilePreviewLease {
    /// The host whose worker owns the root.
    pub host_id: HostId,
    /// Absolute root on that host.
    pub root_path: String,
    /// Wall-clock expiry.
    pub expires_at_ms: u64,
}

#[derive(Default)]
pub struct FilePreviewRegistry {
    leases: Mutex<HashMap<String, FilePreviewLease>>,
}

impl FilePreviewRegistry {
    /// Creates an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a lease and returns its opaque URL id.
    pub fn create(
        &self,
        host_id: HostId,
        root_path: String,
        ttl_ms: u64,
    ) -> Option<(String, FilePreviewLease)> {
        if crate::b5::validate_absolute_path(&root_path).is_err() {
            return None;
        }
        self.purge_expired(now_ms());
        let ttl_ms = ttl_ms.clamp(60_000, 3_600_000);
        let now = now_ms();
        let lease = FilePreviewLease {
            host_id,
            root_path,
            expires_at_ms: now.saturating_add(ttl_ms),
        };
        let id = format!("fprev_{}", EventId::new());
        self.leases
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(id.clone(), lease.clone());
        Some((id, lease))
    }

    /// Removes all expired leases and returns how many were discarded.
    pub fn purge_expired(&self, now_ms: u64) -> usize {
        let mut leases = self
            .leases
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let before = leases.len();
        leases.retain(|_, lease| lease.expires_at_ms > now_ms);
        before.saturating_sub(leases.len())
    }

    /// Returns a live lease, removing expired entries opportunistically.
    pub fn get(&self, id: &str) -> Option<FilePreviewLease> {
        let mut leases = self
            .leases
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let lease = leases.get(id).cloned();
        if lease
            .as_ref()
            .is_some_and(|lease| lease.expires_at_ms <= now_ms())
        {
            leases.remove(id);
            return None;
        }
        lease
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn len(&self) -> usize {
        self.leases
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leases_are_opaque_and_bound_to_one_host_and_root() {
        let registry = FilePreviewRegistry::new();
        let host_id = HostId::mint();
        let (id, lease) = registry
            .create(host_id.clone(), "/srv/project".into(), 60_000)
            .unwrap();
        assert!(id.starts_with("fprev_"));
        assert_eq!(registry.get(&id), Some(lease.clone()));
        assert_eq!(lease.host_id, host_id);
        assert_eq!(lease.root_path, "/srv/project");
    }

    #[test]
    fn a_relative_root_cannot_create_a_lease() {
        let registry = FilePreviewRegistry::new();
        assert!(registry
            .create(HostId::mint(), "relative".into(), 60_000)
            .is_none());
    }

    #[test]
    fn purge_expired_removes_leases_without_a_token_lookup() {
        let registry = FilePreviewRegistry::new();
        let (id, lease) = registry
            .create(HostId::mint(), "/srv/project".into(), 60_000)
            .unwrap();
        assert_eq!(registry.len(), 1);
        assert_eq!(registry.purge_expired(lease.expires_at_ms), 1);
        assert!(registry.get(&id).is_none());
        assert!(registry.is_empty());
    }
}
