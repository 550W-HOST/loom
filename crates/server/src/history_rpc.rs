//! Collecting a thread's restored history from the machine that owns it.
//!
//! The counterpart of [`crate::host_rpc`], for an answer that does not fit one
//! report. The request goes out over the same host scope and carries the same
//! correlation token; the answer comes back as ordered frames:
//!
//! ```text
//! server -- HostRpcRequest{LoadHistory} --> relay host:{id} --> worker
//! server <---- HistoryReport{Chunk}* , {Complete|Failed} ---- worker socket
//! ```
//!
//! The broker is where a stream becomes a value again. It checks that every
//! batch is the one expected, that the terminator agrees with what arrived,
//! and that the whole thing fits the budget the request named. Anything else
//! is a failure — never a short conversation, because a caller that caches
//! half a history cannot tell it apart from a whole one.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use loom_domain::{HostId, HostStatus, ProviderEvent};
use loom_provider_protocol::{HistoryPart, HistoryReport, HostRpcOperation, HostRpcRequest};
use loom_relay::{now_ms, Scope};
use tokio::sync::mpsc;

use crate::state::AppState;

/// Why a history load did not produce a whole conversation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HistoryTransportError {
    /// The request was not a history load.
    NotALoad,
    /// The relay rejected the append.
    Publish(String),
    /// The host did not finish the stream before the deadline.
    Timeout,
    /// The host is enrolled but currently has no worker connection.
    Disconnected(String),
    /// The host is not known to this server.
    UnknownHost(String),
    /// The host reported a failure.
    Failed { code: String, message: String },
    /// The stream ended without a terminator, or its frames did not line up.
    Incomplete(String),
}

impl std::fmt::Display for HistoryTransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotALoad => f.write_str("the request is not a history load"),
            Self::Publish(message) => write!(f, "publish failed: {message}"),
            Self::Timeout => f.write_str("the host did not finish the history load in time"),
            Self::Disconnected(message) => f.write_str(message),
            Self::UnknownHost(message) => f.write_str(message),
            Self::Failed { code, message } => {
                write!(f, "the host could not load history ({code}): {message}")
            }
            Self::Incomplete(message) => write!(f, "the history stream was incomplete: {message}"),
        }
    }
}

struct Pending {
    host_id: HostId,
    sender: mpsc::UnboundedSender<HistoryPart>,
}

/// Correlates streamed history frames with the callers waiting for them.
#[derive(Default)]
pub struct HistoryBroker {
    pending: Mutex<HashMap<String, Pending>>,
}

impl HistoryBroker {
    /// Creates an empty broker.
    pub fn new() -> Self {
        Self::default()
    }

    fn park(&self, request_id: &str, host_id: HostId) -> mpsc::UnboundedReceiver<HistoryPart> {
        let (sender, receiver) = mpsc::unbounded_channel();
        self.lock()
            .insert(request_id.to_owned(), Pending { host_id, sender });
        receiver
    }

    /// Routes one frame to its waiter. Returns whether it belonged to one.
    ///
    /// A mismatched or late frame is dropped, exactly as in the single-answer
    /// broker: a worker cannot answer a request that reused a correlation id
    /// for another host, and a frame arriving after a timeout is harmless. The
    /// waiter is removed on the terminator so a stream cannot outlive its
    /// request.
    pub fn resolve(&self, report: HistoryReport) -> bool {
        let mut pending = self.lock();
        match pending.get(&report.request_id) {
            Some(entry) if entry.host_id == report.host_id => {}
            _ => return false,
        }
        let terminal = matches!(
            report.part,
            HistoryPart::Complete { .. } | HistoryPart::Failed { .. }
        );
        let delivered = pending
            .get(&report.request_id)
            .expect("the pending entry was checked above")
            .sender
            .send(report.part)
            .is_ok();
        if terminal {
            pending.remove(&report.request_id);
        }
        delivered
    }

    /// Removes a request that cannot be answered because publishing failed.
    pub fn forget(&self, request_id: &str) {
        self.lock().remove(request_id);
    }

    /// Number of loads currently waiting.
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
    /// Asks `host_id` to restore a thread's conversation and collects it.
    ///
    /// The bounds travel inside `operation`, so the worker enforces them while
    /// replaying and the server enforces them again while collecting. Neither
    /// side truncates: both fail.
    pub async fn load_thread_history(
        &self,
        host_id: &HostId,
        operation: HostRpcOperation,
        deadline: Duration,
    ) -> Result<Vec<ProviderEvent>, HistoryTransportError> {
        let max_total_bytes = match &operation {
            HostRpcOperation::LoadHistory {
                max_total_bytes, ..
            } => *max_total_bytes,
            _ => return Err(HistoryTransportError::NotALoad),
        };

        let Some(host) = self.registry.host(host_id) else {
            return Err(HistoryTransportError::UnknownHost(format!(
                "host {host_id} is not enrolled on this server"
            )));
        };
        if host.status != HostStatus::Connected {
            return Err(HistoryTransportError::Disconnected(format!(
                "host {host_id} is disconnected"
            )));
        }

        let request_id = format!("hist_{}", loom_relay::EventId::new());
        let request = HostRpcRequest {
            request_id: request_id.clone(),
            host_id: host_id.clone(),
            operation,
            created_at_ms: now_ms(),
        };
        let mut parts = self.history_rpc.park(&request_id, host_id.clone());
        let payload = serde_json::to_vec(&request)
            .expect("a HostRpcRequest made from typed operation data always serializes");
        if let Err(error) = self.publish(Scope::Host(host_id.to_string()), payload) {
            self.history_rpc.forget(&request_id);
            return Err(HistoryTransportError::Publish(error.to_string()));
        }

        let collected = collect(&mut parts, max_total_bytes, deadline).await;
        // A failed or timed-out load leaves nothing behind: the entry is
        // removed either by the terminator or here.
        self.history_rpc.forget(&request_id);
        collected
    }
}

/// Turns the frames of one load back into a conversation.
async fn collect(
    parts: &mut mpsc::UnboundedReceiver<HistoryPart>,
    max_total_bytes: u64,
    deadline: Duration,
) -> Result<Vec<ProviderEvent>, HistoryTransportError> {
    let gather = async {
        let mut entries: Vec<ProviderEvent> = Vec::new();
        let mut bytes: u64 = 0;
        let mut expected: u32 = 0;

        while let Some(part) = parts.recv().await {
            match part {
                HistoryPart::Chunk {
                    batch_index,
                    entries: batch,
                } => {
                    if batch_index != expected {
                        return Err(HistoryTransportError::Incomplete(format!(
                            "batch {batch_index} arrived where {expected} was expected"
                        )));
                    }
                    for entry in batch {
                        let size = serde_json::to_vec(&entry)
                            .map(|encoded| encoded.len() as u64)
                            .unwrap_or(0);
                        if bytes.saturating_add(size) > max_total_bytes {
                            return Err(HistoryTransportError::Incomplete(
                                "the stream exceeded the byte budget the request named".to_owned(),
                            ));
                        }
                        bytes = bytes.saturating_add(size);
                        entries.push(entry);
                    }
                    expected = expected.saturating_add(1);
                }
                HistoryPart::Complete { batch_count } => {
                    if batch_count != expected {
                        return Err(HistoryTransportError::Incomplete(format!(
                            "the host completed after {batch_count} batches but {expected} arrived"
                        )));
                    }
                    return Ok(entries);
                }
                HistoryPart::Failed { code, message } => {
                    return Err(HistoryTransportError::Failed { code, message });
                }
            }
        }

        Err(HistoryTransportError::Incomplete(
            "the host ended the stream without a terminator".to_owned(),
        ))
    };

    match tokio::time::timeout(deadline, gather).await {
        Ok(result) => result,
        Err(_) => Err(HistoryTransportError::Timeout),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use loom_domain::ProviderEvent;

    fn identity() -> ProviderEvent {
        ProviderEvent::ThreadIdentity {
            provider_thread_id: "acp-1".into(),
        }
    }

    fn report(request_id: &str, host_id: HostId, part: HistoryPart) -> HistoryReport {
        HistoryReport {
            host_id,
            request_id: request_id.to_owned(),
            part,
        }
    }

    #[tokio::test]
    async fn a_stream_is_collected_in_batch_order() {
        let broker = HistoryBroker::new();
        let host_id = HostId::mint();
        let mut parts = broker.park("hist_1", host_id.clone());

        assert!(broker.resolve(report(
            "hist_1",
            host_id.clone(),
            HistoryPart::Chunk {
                batch_index: 0,
                entries: vec![identity()],
            },
        )));
        assert!(broker.resolve(report(
            "hist_1",
            host_id.clone(),
            HistoryPart::Chunk {
                batch_index: 1,
                entries: vec![identity()],
            },
        )));
        assert!(broker.resolve(report(
            "hist_1",
            host_id.clone(),
            HistoryPart::Complete { batch_count: 2 },
        )));
        assert_eq!(broker.pending(), 0, "the terminator ends the wait");

        let collected = collect(&mut parts, 1024, Duration::from_secs(5))
            .await
            .expect("a whole stream collects");
        assert_eq!(collected.len(), 2);
    }

    #[tokio::test]
    async fn a_missing_batch_fails_instead_of_shortening_the_conversation() {
        let broker = HistoryBroker::new();
        let host_id = HostId::mint();
        let mut parts = broker.park("hist_2", host_id.clone());
        broker.resolve(report(
            "hist_2",
            host_id.clone(),
            HistoryPart::Chunk {
                batch_index: 0,
                entries: vec![identity()],
            },
        ));
        // Batch 1 never arrives; the terminator claims two batches.
        broker.resolve(report(
            "hist_2",
            host_id.clone(),
            HistoryPart::Complete { batch_count: 2 },
        ));

        let failure = collect(&mut parts, 1024, Duration::from_secs(5))
            .await
            .expect_err("a gap is a failure");
        assert!(matches!(failure, HistoryTransportError::Incomplete(_)));
    }

    #[tokio::test]
    async fn an_out_of_order_batch_fails() {
        let broker = HistoryBroker::new();
        let host_id = HostId::mint();
        let mut parts = broker.park("hist_3", host_id.clone());
        broker.resolve(report(
            "hist_3",
            host_id.clone(),
            HistoryPart::Chunk {
                batch_index: 1,
                entries: vec![identity()],
            },
        ));

        let failure = collect(&mut parts, 1024, Duration::from_secs(5))
            .await
            .expect_err("batch 1 before batch 0 is a failure");
        assert!(matches!(failure, HistoryTransportError::Incomplete(_)));
    }

    #[tokio::test]
    async fn a_host_failure_is_reported_as_a_failure() {
        let broker = HistoryBroker::new();
        let host_id = HostId::mint();
        let mut parts = broker.park("hist_4", host_id.clone());
        broker.resolve(report(
            "hist_4",
            host_id.clone(),
            HistoryPart::Failed {
                code: "session_missing".into(),
                message: "no such session".into(),
            },
        ));

        let failure = collect(&mut parts, 1024, Duration::from_secs(5))
            .await
            .expect_err("a host failure is not an empty conversation");
        assert_eq!(
            failure,
            HistoryTransportError::Failed {
                code: "session_missing".into(),
                message: "no such session".into(),
            }
        );
        assert_eq!(broker.pending(), 0);
    }

    #[tokio::test]
    async fn a_report_from_another_host_is_dropped() {
        let broker = HistoryBroker::new();
        let host_id = HostId::mint();
        let _parts = broker.park("hist_5", host_id);
        assert!(!broker.resolve(report(
            "hist_5",
            HostId::mint(),
            HistoryPart::Complete { batch_count: 0 },
        )));
        assert_eq!(broker.pending(), 1);
    }
}
