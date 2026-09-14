//! Host-owned workspace requests.
//!
//! Workspace state belongs to the machine that owns an environment. The
//! control plane therefore publishes a request to that host's relay scope and
//! waits for the daemon to answer on its enrolled socket:
//!
//! ```text
//! server -- HostRpcRequest --> relay host:{id} --> daemon
//! server <-- HostRpcReport -- daemon socket
//! ```
//!
//! The broker is intentionally separate from the relay. A relay publisher
//! does not know which connection will receive a frame; the correlation map is
//! only the short-lived HTTP wait state needed to return one answer to one
//! caller.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use loom_domain::{HostId, HostStatus};
use loom_provider_protocol::{HostRpcOperation, HostRpcOutcome, HostRpcReport, HostRpcRequest};
use loom_relay::{now_ms, Scope};
use tokio::sync::oneshot;

use crate::state::AppState;

/// How long a daemon has to answer one workspace operation.
pub const HOST_RPC_TIMEOUT: Duration = Duration::from_secs(30);

/// Why a workspace request did not reach a usable report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostRpcTransportError {
    /// The relay rejected the append.
    Publish(String),
    /// The daemon did not answer before [`HOST_RPC_TIMEOUT`].
    Timeout,
    /// The host is enrolled but currently has no daemon connection.
    Disconnected(String),
    /// The host is not known to this server.
    UnknownHost(String),
}

impl std::fmt::Display for HostRpcTransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Publish(message) => write!(f, "publish failed: {message}"),
            Self::Timeout => write!(f, "the host did not answer the workspace request in time"),
            Self::Disconnected(message) => write!(f, "{message}"),
            Self::UnknownHost(message) => f.write_str(message),
        }
    }
}

struct Pending {
    host_id: HostId,
    sender: oneshot::Sender<HostRpcReport>,
}

/// Correlates host reports with waiting HTTP requests.
#[derive(Default)]
pub struct HostRpcBroker {
    pending: Mutex<HashMap<String, Pending>>,
}

impl HostRpcBroker {
    /// Creates an empty broker.
    pub fn new() -> Self {
        Self::default()
    }

    fn park(&self, request_id: &str, host_id: HostId) -> oneshot::Receiver<HostRpcReport> {
        let (sender, receiver) = oneshot::channel();
        self.lock()
            .insert(request_id.to_owned(), Pending { host_id, sender });
        receiver
    }

    /// Resolves a report if it belongs to the host that was asked.
    ///
    /// A mismatched or late report is dropped. In particular, a daemon cannot
    /// answer a request that happened to reuse a correlation id for another
    /// host, and a report arriving after an HTTP timeout is harmless.
    pub fn resolve(&self, report: HostRpcReport) -> bool {
        let mut pending = self.lock();
        let Some(entry) = pending.get(&report.request_id) else {
            return false;
        };
        if entry.host_id != report.host_id {
            return false;
        }
        let entry = pending
            .remove(&report.request_id)
            .expect("the pending report was checked above");
        entry.sender.send(report).is_ok()
    }

    /// Removes a request that cannot be answered because publishing failed.
    pub fn forget(&self, request_id: &str) {
        self.lock().remove(request_id);
    }

    /// Number of requests currently waiting.
    pub fn pending(&self) -> usize {
        self.lock().len()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Pending>> {
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl AppState {
    /// Publishes a workspace request to `host_id` and waits for its report.
    pub async fn request_host_rpc(
        &self,
        host_id: &HostId,
        operation: HostRpcOperation,
    ) -> Result<HostRpcOutcome, HostRpcTransportError> {
        let Some(host) = self.registry.host(host_id) else {
            return Err(HostRpcTransportError::UnknownHost(format!(
                "host {host_id} is not enrolled on this server"
            )));
        };
        if host.status != HostStatus::Connected {
            return Err(HostRpcTransportError::Disconnected(format!(
                "host {host_id} is disconnected"
            )));
        }

        let request_id = format!("hrpc_{}", loom_relay::EventId::new());
        let request = HostRpcRequest {
            request_id: request_id.clone(),
            host_id: host_id.clone(),
            operation,
            created_at_ms: now_ms(),
        };
        let waiter = self.host_rpc.park(&request_id, host_id.clone());
        let payload = serde_json::to_vec(&request)
            .expect("a HostRpcRequest made from typed operation data always serializes");
        if let Err(error) = self.publish(Scope::Host(host_id.to_string()), payload) {
            self.host_rpc.forget(&request_id);
            return Err(HostRpcTransportError::Publish(error.to_string()));
        }

        match tokio::time::timeout(HOST_RPC_TIMEOUT, waiter).await {
            Ok(Ok(report)) => Ok(report.outcome),
            Ok(Err(_)) | Err(_) => {
                // A timeout drops the receiver, but the sender lives in the
                // broker until it is explicitly removed. Forget it here so a
                // disconnected or wedged daemon cannot accumulate entries.
                self.host_rpc.forget(&request_id);
                Err(HostRpcTransportError::Timeout)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(request_id: &str, host_id: HostId) -> HostRpcReport {
        HostRpcReport {
            host_id,
            request_id: request_id.to_owned(),
            outcome: HostRpcOutcome::Result {
                result: serde_json::json!({ "ok": true }),
            },
        }
    }

    #[tokio::test]
    async fn a_report_wakes_only_the_matching_host_waiter() {
        let broker = HostRpcBroker::new();
        let host_id = HostId::mint();
        let waiter = broker.park("req-1", host_id.clone());
        assert!(!broker.resolve(report("req-1", HostId::mint())));
        assert_eq!(broker.pending(), 1);
        assert!(broker.resolve(report("req-1", host_id)));
        assert_eq!(waiter.await.unwrap().request_id, "req-1");
        assert_eq!(broker.pending(), 0);
    }

    #[tokio::test]
    async fn a_late_report_is_dropped() {
        let broker = HostRpcBroker::new();
        assert!(!broker.resolve(report("never-asked", HostId::mint())));
        assert_eq!(broker.pending(), 0);
    }
}
