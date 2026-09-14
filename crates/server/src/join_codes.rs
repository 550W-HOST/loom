//! One-time host enrollment codes.
//!
//! Codes are deliberately process-local capabilities. They are not persisted
//! with domain state or shared through the relay, so a server restart or a
//! request routed to another node invalidates a code. The registry is suitable
//! for the default single-node deployment; a shared enrollment store is needed
//! before placing code issuance and enrollment behind independent nodes.

use std::collections::HashMap;
use std::sync::Mutex;

use loom_domain::HostId;
use loom_relay::{now_ms, EventId};

const JOIN_CODE_TTL_MS: u64 = 10 * 60 * 1_000;

#[derive(Clone, Debug)]
struct PendingJoin {
    host_id: HostId,
    expires_at_ms: u64,
}

#[derive(Default)]
pub struct JoinCodeRegistry {
    pending: Mutex<HashMap<String, PendingJoin>>,
}

impl JoinCodeRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Issues a code and reserves the host identity it represents.
    pub fn issue(&self) -> (String, HostId, u64) {
        self.purge_expired(now_ms());
        let code = format!("loom-{}", EventId::new());
        let host_id = HostId::mint();
        let expires_at_ms = now_ms().saturating_add(JOIN_CODE_TTL_MS);
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(
                code.clone(),
                PendingJoin {
                    host_id: host_id.clone(),
                    expires_at_ms,
                },
            );
        (code, host_id, expires_at_ms)
    }

    /// Removes all expired codes and returns how many were discarded.
    pub fn purge_expired(&self, now_ms: u64) -> usize {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let before = pending.len();
        pending.retain(|_, join| join.expires_at_ms > now_ms);
        before.saturating_sub(pending.len())
    }

    /// Consumes a live code exactly once.
    pub fn consume(&self, code: &str) -> Option<HostId> {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let join = pending.remove(code)?;
        (join.expires_at_ms > now_ms()).then_some(join.host_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn purge_expired_removes_codes_without_a_token_lookup() {
        let registry = JoinCodeRegistry::new();
        let (code, _, expires_at_ms) = registry.issue();
        assert_eq!(registry.purge_expired(expires_at_ms), 1);
        assert!(registry.consume(&code).is_none());
    }

    #[test]
    fn a_code_is_consumed_exactly_once() {
        let registry = JoinCodeRegistry::new();
        let (code, host_id, _) = registry.issue();
        assert_eq!(registry.consume(&code), Some(host_id));
        assert!(registry.consume(&code).is_none());
    }
}
