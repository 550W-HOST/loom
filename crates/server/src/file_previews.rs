//! Short-lived, host-bound file preview leases.
//!
//! A preview URL is a capability returned by the future `files.createPreview`
//! route. The URL must never turn a client-controlled host id and absolute path
//! into an arbitrary filesystem read, so the lease binds the request to one
//! host and one root. The B8 content route only accepts a lease token.

use std::collections::HashMap;
use std::sync::Mutex;

use loom_domain::HostId;
use loom_relay::{now_ms, EventId};

/// A root-bound preview capability.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FilePreviewLease {
    /// The host whose daemon owns the root.
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
        if root_path.is_empty() || !root_path.starts_with('/') {
            return None;
        }
        let ttl_ms = ttl_ms.clamp(60_000, 3_600_000);
        let lease = FilePreviewLease {
            host_id,
            root_path,
            expires_at_ms: now_ms().saturating_add(ttl_ms),
        };
        let id = format!("fprev_{}", EventId::new());
        self.leases
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(id.clone(), lease.clone());
        Some((id, lease))
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
}
