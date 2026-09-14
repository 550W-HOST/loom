//! Host file requests: asking the machine that owns a thread's files.
//!
//! A thread's workspace and its thread storage live on the host its environment
//! names. The control plane must never read its *own* disk and present the
//! result as a host file — that is the failure this module exists to make
//! impossible — so every read and listing is a request to that host:
//!
//! ```text
//!   server ── HostFileRequest ──▶ relay host:{id} ──▶ daemon
//!   server ◀── HostFileReport ── daemon socket (ClientCommand::HostFileReport)
//! ```
//!
//! The request goes through the relay for the same reason a dispatch does: a
//! daemon that was reconnecting receives it on replay, and replaying a read is
//! harmless because reading is idempotent. The answer comes back up the
//! daemon's own socket because it satisfies exactly one waiting HTTP request;
//! fanning it out to the host's room would deliver a file's contents to every
//! client watching that room for no reason.
//!
//! # Why a broker
//!
//! The relay is one-way by design — a producer never learns who is subscribed,
//! and a handler never touches a connection. So the waiting HTTP request is
//! parked here under a fresh correlation token and woken by the socket task
//! when the report arrives. A report whose token is unknown is dropped: that is
//! what makes a redelivered or timed-out request a no-op rather than a panic.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use loom_domain::HostId;
use loom_provider_protocol::{HostFileOperation, HostFileOutcome, HostFileReport, HostFileRequest};
use loom_relay::{now_ms, Scope};
use tokio::sync::oneshot;

use crate::state::AppState;

/// How long a host has to answer before a read is given up on.
///
/// Matches bb's `COMMAND_TIMEOUT_MS`: the daemon's own filesystem work is fast,
/// so anything approaching this is a host that is connected but wedged, and
/// holding the client's request open longer would be a connection leak with a
/// friendly name.
pub const HOST_FILE_TIMEOUT: Duration = Duration::from_secs(30);

/// Why a host file request did not produce a result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostFileTransportError {
    /// The request could not be appended to the log, so no host can see it.
    Publish(String),
    /// The host did not answer within [`HOST_FILE_TIMEOUT`].
    Timeout,
    /// The host is not enrolled on this server at all.
    UnknownHost(String),
}

impl std::fmt::Display for HostFileTransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HostFileTransportError::Publish(message) => write!(f, "publish failed: {message}"),
            HostFileTransportError::Timeout => {
                write!(f, "the host did not answer the file request in time")
            }
            HostFileTransportError::UnknownHost(message) => write!(f, "{message}"),
        }
    }
}

/// Parks the HTTP requests waiting on a host answer.
///
/// One entry per in-flight request. Nothing is retained after a request
/// settles, so a burst of reads does not accumulate state.
#[derive(Default)]
pub struct HostFileBroker {
    pending: Mutex<HashMap<String, oneshot::Sender<HostFileReport>>>,
}

impl HostFileBroker {
    /// An empty broker.
    pub fn new() -> Self {
        Self::default()
    }

    /// Parks a waiter under `request_id`.
    fn park(&self, request_id: &str) -> oneshot::Receiver<HostFileReport> {
        let (sender, receiver) = oneshot::channel();
        self.lock().insert(request_id.to_owned(), sender);
        receiver
    }

    /// Wakes the waiter for a report, returning whether one was waiting.
    ///
    /// `false` is the normal outcome for a report this server is not tracking:
    /// a redelivery after a timeout, or an answer to a request whose client
    /// already gave up. It is not an error.
    pub fn resolve(&self, report: HostFileReport) -> bool {
        let waiter = self.lock().remove(&report.request_id);
        match waiter {
            Some(sender) => sender.send(report).is_ok(),
            None => false,
        }
    }

    /// Drops a waiter that will never be answered.
    ///
    /// Used when the request never reached the log, so no host can see it and
    /// waiting the full timeout would only delay an error the caller already
    /// has. Dropping the sender wakes the receiver immediately with a closed
    /// channel, which [`AppState::request_host_file`] reports as a timeout.
    pub fn forget(&self, request_id: &str) {
        self.lock().remove(request_id);
    }

    /// Number of requests still waiting, for tests and diagnostics.
    pub fn pending(&self) -> usize {
        self.lock().len()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, oneshot::Sender<HostFileReport>>> {
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl AppState {
    /// Asks `host_id` to perform a filesystem operation and waits for its
    /// answer.
    ///
    /// This is the only way the control plane reads a host file, which is what
    /// keeps a server-local path out of every file response. A host that is not
    /// enrolled is refused before anything is published, because a request into
    /// a room nobody is in would only time out.
    pub async fn request_host_file(
        &self,
        host_id: &HostId,
        operation: HostFileOperation,
    ) -> Result<HostFileOutcome, HostFileTransportError> {
        if self.registry.host(host_id).is_none() {
            return Err(HostFileTransportError::UnknownHost(format!(
                "host {host_id} is not enrolled on this server"
            )));
        }

        // A correlation token, not a domain id: the server mints it, waits on
        // exactly this value, and drops any report it no longer recognises. The
        // relay's monotonic id is the natural source — it is already the
        // process's unique, sortable token mint.
        let request_id = format!("hfil_{}", loom_relay::EventId::new());
        let request = HostFileRequest {
            request_id: request_id.clone(),
            host_id: host_id.clone(),
            operation,
            created_at_ms: now_ms(),
        };
        let waiter = self.host_files.park(&request_id);

        let payload =
            serde_json::to_vec(&request).expect("a HostFileRequest always serializes to JSON");
        if let Err(error) = self.publish(Scope::Host(host_id.to_string()), payload) {
            // Nothing reached the log, so no host can answer. Drop the waiter
            // rather than leave it parked for the full timeout, and report the
            // reason the caller actually needs.
            self.host_files.forget(&request_id);
            return Err(HostFileTransportError::Publish(error.to_string()));
        }

        match tokio::time::timeout(HOST_FILE_TIMEOUT, waiter).await {
            Ok(Ok(report)) => Ok(report.outcome),
            // The sender was dropped without answering; treat it as a timeout
            // rather than a distinct failure, because from the client's side
            // the two are the same "no answer".
            Ok(Err(_)) | Err(_) => Err(HostFileTransportError::Timeout),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use loom_domain::HostId;

    fn report(request_id: &str) -> HostFileReport {
        HostFileReport {
            host_id: HostId::mint(),
            request_id: request_id.to_owned(),
            outcome: HostFileOutcome::Failed {
                code: "not_found".into(),
                message: "no such file".into(),
            },
        }
    }

    #[tokio::test]
    async fn a_parked_request_is_woken_by_its_report() {
        let broker = HostFileBroker::new();
        let waiter = broker.park("req-1");
        assert_eq!(broker.pending(), 1);
        assert!(broker.resolve(report("req-1")));
        assert_eq!(broker.pending(), 0);
        let answered = waiter.await.unwrap();
        assert_eq!(answered.request_id, "req-1");
    }

    #[tokio::test]
    async fn an_answer_to_nothing_is_dropped_not_a_panic() {
        let broker = HostFileBroker::new();
        assert!(!broker.resolve(report("never-asked")));
        assert_eq!(broker.pending(), 0);
    }

    #[tokio::test]
    async fn forgetting_a_request_wakes_it_instead_of_leaving_it_parked() {
        let broker = HostFileBroker::new();
        let waiter = broker.park("req-gone");
        broker.forget("req-gone");
        assert_eq!(broker.pending(), 0);
        // A dropped sender is what `request_host_file` reports as a timeout, so
        // a publish failure never costs the caller the full wait.
        assert!(waiter.await.is_err());
    }
}
