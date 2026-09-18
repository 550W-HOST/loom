//! Terminal requests to the host that owns a session.
//!
//! A terminal's process lives on exactly one machine, and the control plane
//! never holds a PTY handle of its own. Driving a session is therefore a request
//! to that machine, using the same relay-plus-report shape as a host file read:
//!
//! ```text
//!   server ── TerminalRequest ──▶ relay host:{id} ──▶ worker
//!   server ◀── TerminalReport ── worker socket
//! ```
//!
//! The request travels through the relay so a worker that was reconnecting
//! still receives it on replay. The answer comes back up the worker's own
//! socket because it satisfies exactly one waiting HTTP request; fanning it out
//! to the host's room would deliver one terminal's bytes to every client
//! watching that room.
//!
//! # Why a broker
//!
//! The relay is one-way: a producer never learns who is subscribed, and a
//! handler never touches a connection. So the waiting HTTP request is parked
//! here under a fresh correlation token and woken when the report arrives. A
//! report whose token is unknown is dropped — that is what makes a redelivered
//! or timed-out request a no-op rather than a panic.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use loom_domain::{HostId, HostStatus, ThreadId};
use loom_provider_protocol::{
    TerminalCloseReason, TerminalOperation, TerminalOutcome, TerminalReport, TerminalRequest,
    TerminalSession, TerminalStatus,
};
use loom_relay::{now_ms, Scope};
use tokio::sync::oneshot;

use crate::state::AppState;

/// How long a host has to answer one terminal operation.
///
/// Matches the other host RPCs. A terminal operation is fast (a write to a pty,
/// a resize ioctl, a ring read); anything approaching this is a host that is
/// connected but wedged, and holding the client's request open longer would be
/// a connection leak with a friendly name.
pub const TERMINAL_TIMEOUT: Duration = Duration::from_secs(30);

/// Why a terminal request did not produce a result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TerminalTransportError {
    /// The relay rejected the append.
    Publish(String),
    /// The host did not answer before [`TERMINAL_TIMEOUT`].
    Timeout,
    /// The host is enrolled but currently has no worker connection.
    Disconnected(String),
    /// The host is not known to this server.
    UnknownHost(String),
}

impl std::fmt::Display for TerminalTransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Publish(message) => write!(f, "publish failed: {message}"),
            Self::Timeout => write!(f, "the host did not answer the terminal request in time"),
            Self::Disconnected(message) => f.write_str(message),
            Self::UnknownHost(message) => f.write_str(message),
        }
    }
}

/// The control plane's view of every terminal session it has minted.
///
/// The **worker** is the authority on whether a process is alive; this is the
/// routing and listing index. It holds only identity, ownership and size — never
/// output, which lives in the worker's bounded ring and is only ever read
/// through a cursor.
#[derive(Default)]
pub struct TerminalSessions {
    sessions: Mutex<HashMap<String, TerminalSession>>,
}

impl TerminalSessions {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Mints a fresh session id.
    ///
    /// Not a [`loom_domain`] typed id: the contract's `terminalId` is an opaque
    /// string, and a new entity type would ripple through the id crate for a
    /// value no other layer needs to parse.
    pub fn mint_id() -> String {
        format!("term_{}", loom_relay::EventId::new())
    }

    /// Inserts or replaces a session record.
    pub fn put(&self, session: TerminalSession) {
        self.lock().insert(session.id.clone(), session);
    }

    /// Reads one session.
    pub fn get(&self, id: &str) -> Option<TerminalSession> {
        self.lock().get(id).cloned()
    }

    /// Removes one session, returning it when it was there.
    pub fn remove(&self, id: &str) -> Option<TerminalSession> {
        self.lock().remove(id)
    }

    /// Every session, newest first.
    pub fn list(&self) -> Vec<TerminalSession> {
        let mut sessions: Vec<TerminalSession> = self.lock().values().cloned().collect();
        sessions.sort_by(|left, right| {
            right
                .created_at_ms
                .cmp(&left.created_at_ms)
                .then_with(|| right.id.cmp(&left.id))
        });
        sessions
    }

    /// How many sessions are recorded.
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// Whether nothing is recorded.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Marks every live session on `host_id` as disconnected.
    ///
    /// Called when its worker drops: the process may well still be alive on
    /// that machine, but no client can reach it through a server with no
    /// connection to the host, and reporting `running` would be a status the
    /// user cannot act on.
    pub fn mark_host_disconnected(&self, host_id: &HostId, now_ms: u64) -> usize {
        let mut sessions = self.lock();
        let mut changed = 0;
        for session in sessions.values_mut() {
            if &session.host_id != host_id {
                continue;
            }
            if matches!(
                session.status,
                TerminalStatus::Starting | TerminalStatus::Running
            ) {
                session.status = TerminalStatus::Disconnected;
                session.updated_at_ms = now_ms;
                changed += 1;
            }
        }
        changed
    }

    /// Settles every session on a thread that no longer exists.
    ///
    /// A terminal belongs to the thread it was opened from; when that thread is
    /// deleted or archived there is no client left to render its output, so the
    /// record is closed here. The worker kills the process when it receives the
    /// matching close request.
    pub fn threads_to_close(
        &self,
        thread_id: &ThreadId,
        reason: TerminalCloseReason,
        now_ms: u64,
    ) -> Vec<String> {
        let mut sessions = self.lock();
        let mut closing = Vec::new();
        for session in sessions.values_mut() {
            if session.thread_id.as_ref() != Some(thread_id) {
                continue;
            }
            if matches!(
                session.status,
                TerminalStatus::Starting | TerminalStatus::Running | TerminalStatus::Disconnected
            ) {
                session.status = TerminalStatus::Exited;
                session.close_reason = Some(reason);
                session.updated_at_ms = now_ms;
                closing.push(session.id.clone());
            }
        }
        closing
    }

    /// Settles every session on an environment that no longer exists.
    pub fn environments_to_close(
        &self,
        environment_id: &loom_domain::EnvironmentId,
        reason: TerminalCloseReason,
        now_ms: u64,
    ) -> Vec<String> {
        let mut sessions = self.lock();
        let mut closing = Vec::new();
        for session in sessions.values_mut() {
            if session.environment_id.as_ref() != Some(environment_id) {
                continue;
            }
            if matches!(
                session.status,
                TerminalStatus::Starting | TerminalStatus::Running | TerminalStatus::Disconnected
            ) {
                session.status = TerminalStatus::Exited;
                session.close_reason = Some(reason);
                session.updated_at_ms = now_ms;
                closing.push(session.id.clone());
            }
        }
        closing
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, TerminalSession>> {
        self.sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Parks the HTTP requests waiting on a terminal answer.
#[derive(Default)]
pub struct TerminalBroker {
    pending: Mutex<HashMap<String, oneshot::Sender<TerminalReport>>>,
}

impl TerminalBroker {
    /// An empty broker.
    pub fn new() -> Self {
        Self::default()
    }

    fn park(&self, request_id: &str) -> oneshot::Receiver<TerminalReport> {
        let (sender, receiver) = oneshot::channel();
        self.lock().insert(request_id.to_owned(), sender);
        receiver
    }

    /// Wakes the waiter for a report, returning whether one was waiting.
    ///
    /// `false` is the normal outcome for a report this server is not tracking:
    /// a redelivery after a timeout, or an answer to a request whose client
    /// already gave up.
    pub fn resolve(&self, report: TerminalReport) -> bool {
        let waiter = self.lock().remove(&report.request_id);
        match waiter {
            Some(sender) => sender.send(report).is_ok(),
            None => false,
        }
    }

    /// Drops a waiter that will never be answered.
    pub fn forget(&self, request_id: &str) {
        self.lock().remove(request_id);
    }

    /// Number of requests still waiting, for tests and diagnostics.
    pub fn pending(&self) -> usize {
        self.lock().len()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, oneshot::Sender<TerminalReport>>> {
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl AppState {
    /// Reconciles the control plane's terminal records with the host that owns
    /// them.
    ///
    /// The worker is the authority on whether a process is alive: it survives a
    /// dropped server connection, and a terminal is the user's, not the
    /// connection's. So after a host reconnects, every session recorded for it
    /// is re-checked against the machine rather than assumed dead. A session the
    /// host no longer holds is closed here; one it still holds is returned to
    /// `running`.
    ///
    /// Runs on a task: each answer waits on a relay round trip, and enrollment
    /// must not block on one.
    pub fn spawn_terminal_reconcile(&self, host_id: HostId) {
        let state = self.clone();
        tokio::spawn(async move {
            // One full inventory per reconnect: the host is the authority on
            // which of its processes are alive, and a server that just started
            // has no records at all — asking per recorded id could never learn
            // about a session it never saw.
            let recorded: Vec<String> = state
                .terminals
                .list()
                .into_iter()
                .filter(|session| session.host_id == host_id)
                .map(|session| session.id)
                .collect();
            let inventory = state
                .request_terminal(&host_id, TerminalOperation::Report { id: None })
                .await;
            let mut live: Vec<String> = Vec::new();
            match inventory {
                Ok(TerminalOutcome::Sessions { sessions }) => {
                    for session in sessions {
                        live.push(session.id.clone());
                        // Sessions the server already knows are refreshed; ones
                        // it does not are adopted, so a server restart does not
                        // orphan a still-running terminal.
                        state.terminals.put(session);
                    }
                }
                // A host that cannot be reached right now is not evidence that
                // any process ended: leave the records as they are and let the
                // next reconnect try again.
                Ok(_) | Err(_) => return,
            }
            for id in recorded {
                if !live.contains(&id) {
                    if let Some(mut session) = state.terminals.get(&id) {
                        session.status = TerminalStatus::Exited;
                        session.close_reason = Some(TerminalCloseReason::WorkerDisconnect);
                        session.updated_at_ms = now_ms();
                        state.terminals.put(session);
                    }
                }
            }
        });
    }

    /// Asks `host_id` to perform one terminal operation and waits for its
    /// answer.
    pub async fn request_terminal(
        &self,
        host_id: &HostId,
        operation: TerminalOperation,
    ) -> Result<TerminalOutcome, TerminalTransportError> {
        let Some(host) = self.registry.host(host_id) else {
            return Err(TerminalTransportError::UnknownHost(format!(
                "host {host_id} is not enrolled on this server"
            )));
        };
        if host.status != HostStatus::Connected {
            return Err(TerminalTransportError::Disconnected(format!(
                "host {host_id} is disconnected"
            )));
        }

        let request_id = format!("term_{}", loom_relay::EventId::new());
        let request = TerminalRequest {
            request_id: request_id.clone(),
            host_id: host_id.clone(),
            operation,
            created_at_ms: now_ms(),
        };
        let waiter = self.terminal.park(&request_id);

        let payload =
            serde_json::to_vec(&request).expect("a TerminalRequest always serializes to JSON");
        if let Err(error) = self.publish(Scope::Host(host_id.to_string()), payload) {
            self.terminal.forget(&request_id);
            return Err(TerminalTransportError::Publish(error.to_string()));
        }

        match tokio::time::timeout(TERMINAL_TIMEOUT, waiter).await {
            Ok(Ok(report)) => Ok(report.outcome),
            Ok(Err(_)) | Err(_) => {
                self.terminal.forget(&request_id);
                Err(TerminalTransportError::Timeout)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(request_id: &str) -> TerminalReport {
        TerminalReport {
            host_id: HostId::mint(),
            request_id: request_id.to_owned(),
            outcome: TerminalOutcome::Sessions {
                sessions: Vec::new(),
            },
        }
    }

    #[tokio::test]
    async fn a_parked_request_is_woken_by_its_report() {
        let broker = TerminalBroker::new();
        let waiter = broker.park("req-1");
        assert_eq!(broker.pending(), 1);
        assert!(broker.resolve(report("req-1")));
        assert_eq!(broker.pending(), 0);
        assert_eq!(waiter.await.unwrap().request_id, "req-1");
    }

    #[tokio::test]
    async fn an_answer_to_nothing_is_dropped_not_a_panic() {
        let broker = TerminalBroker::new();
        assert!(!broker.resolve(report("never-asked")));
        assert_eq!(broker.pending(), 0);
    }
}
