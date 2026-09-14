//! One-time host enrollment codes.

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
