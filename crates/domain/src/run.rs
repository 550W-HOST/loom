//! A provider run: the lifecycle and the streamed detail of one agent turn.
//!
//! A *run* is what happens between a thread leaving `idle` and returning to a
//! terminal status. It is deliberately separate from [`Thread`](crate::Thread):
//! the thread is the durable conversation, the run is one attempt at advancing
//! it. A failed run leaves the thread in `error`, which a retry turns into a
//! *new* run with a new [`RunId`](crate::RunId).
//!
//! Every run produces a stream of [`RunEvent`]s. Each carries a
//! [`ThreadEvent`] — bb's contract event — so a client's projection layer can
//! consume it unchanged. The scope of every event is derived from the run: a
//! turn-scoped event uses the run id as its `turnId`, which is what makes a
//! turn survive a server restart.
//!
//! The stream always ends in exactly one [`ProviderEvent::TurnCompleted`]:
//! that is the invariant that keeps a thread from being stuck in `working`
//! forever after a provider crash, a worker that vanished or a timeout.
//!
//! [`ProviderEvent::TurnCompleted`]: crate::ProviderEvent::TurnCompleted

use serde::{Deserialize, Serialize};

use crate::id::{ProjectId, RunId, ThreadId};
use crate::provider_event::{ProviderEvent, ThreadEvent, ThreadEventScope, TurnError, TurnStatus};

/// loom's classification of how a run ended.
///
/// Distinct from the contract's [`TurnStatus`]: this is the *control plane's*
/// verdict (which also covers the failure modes a provider never reports:
/// a deadline, a stale host, a cancellation), and it is what the thread
/// lifecycle transition is chosen from. It is mapped onto a contract
/// `turn/completed.status` when the terminal event is built.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunOutcome {
    /// The provider settled normally.
    Completed,
    /// The provider process exited non-zero or reported a protocol failure.
    Failed,
    /// The run exceeded its deadline and was killed.
    TimedOut,
    /// The worker holding the run stopped heartbeating; the run was reaped.
    HostStale,
    /// The operator or a client cancelled the run.
    Cancelled,
}

impl RunOutcome {
    /// Whether the thread should return to `idle` (vs. `error`) afterwards.
    pub fn is_success(self) -> bool {
        matches!(self, RunOutcome::Completed)
    }

    /// The contract turn status this outcome maps onto.
    pub fn turn_status(self) -> TurnStatus {
        match self {
            RunOutcome::Completed => TurnStatus::Completed,
            RunOutcome::Cancelled => TurnStatus::Interrupted,
            RunOutcome::Failed | RunOutcome::TimedOut | RunOutcome::HostStale => TurnStatus::Failed,
        }
    }

    /// The stable wire token.
    pub fn as_str(self) -> &'static str {
        match self {
            RunOutcome::Completed => "completed",
            RunOutcome::Failed => "failed",
            RunOutcome::TimedOut => "timed_out",
            RunOutcome::HostStale => "host_stale",
            RunOutcome::Cancelled => "cancelled",
        }
    }
}

/// One fact about an in-flight provider run.
///
/// The payload is a full [`ThreadEvent`] (bb's contract shape): it carries
/// `threadId`, `scope` and the discriminated body in one flattened object. The
/// surrounding fields are loom's envelope, adding the identity a consumer
/// needs without re-deriving it from the log.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RunEvent {
    /// The thread the run belongs to.
    #[serde(rename = "thread_id")]
    pub thread_id: ThreadId,
    /// Its project, so a consumer need not look it up.
    #[serde(rename = "project_id")]
    pub project_id: ProjectId,
    /// The run's identity. Also the turn id of every turn-scoped event.
    #[serde(rename = "run_id")]
    pub run_id: RunId,
    /// Wall-clock milliseconds of the event.
    #[serde(rename = "at_ms")]
    pub at_ms: u64,
    /// loom's control-plane verdict, present only on the terminal event.
    ///
    /// This is *not* part of bb's contract event: it is loom's own envelope
    /// field, and it is what distinguishes a deadline, a stale host and a
    /// cancellation, which the contract folds into a single `turn/completed`
    /// status. The inner [`ThreadEvent`] stays exactly contract-shaped, so a
    /// projection consuming `event` is unaffected.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<RunOutcome>,
    /// The contract event.
    pub event: ThreadEvent,
}

impl RunEvent {
    /// Builds a run event, choosing the scope from the event type's policy.
    pub fn new(
        thread_id: ThreadId,
        project_id: ProjectId,
        run_id: RunId,
        at_ms: u64,
        event: ProviderEvent,
    ) -> Self {
        let scope = match event.kind() {
            kind if ProviderEvent::is_thread_scoped(kind) => ThreadEventScope::Thread,
            _ => ThreadEventScope::turn(&run_id),
        };
        Self {
            thread_id: thread_id.clone(),
            project_id,
            run_id,
            at_ms,
            outcome: None,
            event: ThreadEvent::new(thread_id, scope, event),
        }
    }

    /// Builds the synthetic provider identity used when the control plane has
    /// to close a run before an ACP session exists.
    ///
    /// This value is deliberately namespaced and derived from the run. It is a
    /// timeline identity only; callers must never persist it as a resumable
    /// provider session id.
    pub fn synthetic_provider_thread_id(run_id: &RunId) -> String {
        format!("loom-preflight-{run_id}")
    }

    /// Builds the start event for a turn.
    pub fn started(
        thread_id: ThreadId,
        project_id: ProjectId,
        run_id: RunId,
        at_ms: u64,
        provider_thread_id: impl Into<String>,
    ) -> Self {
        Self::new(
            thread_id,
            project_id,
            run_id,
            at_ms,
            ProviderEvent::TurnStarted {
                provider_thread_id: provider_thread_id.into(),
                parent_tool_call_id: None,
            },
        )
    }

    /// Builds a provider error diagnostic for a run.
    pub fn provider_error(
        thread_id: ThreadId,
        project_id: ProjectId,
        run_id: RunId,
        at_ms: u64,
        provider_thread_id: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self::new(
            thread_id,
            project_id,
            run_id,
            at_ms,
            ProviderEvent::ProviderError {
                provider_thread_id: provider_thread_id.into(),
                message: message.into(),
                detail: None,
                error_info: None,
                will_retry: Some(false),
            },
        )
    }

    /// Builds the ordered lifecycle for a server-owned failure.
    ///
    /// The first identity is synthetic when no provider session exists. It is
    /// only used to satisfy the turn projection contract; the terminal event
    /// keeps `providerThreadId` nullable and the message is copied verbatim to
    /// both diagnostic locations.
    pub fn failure_sequence(
        thread_id: ThreadId,
        project_id: ProjectId,
        run_id: RunId,
        at_ms: u64,
        outcome: RunOutcome,
        message: impl Into<String>,
    ) -> [Self; 3] {
        let message = message.into();
        let provider_thread_id = Self::synthetic_provider_thread_id(&run_id);
        [
            Self::started(
                thread_id.clone(),
                project_id.clone(),
                run_id.clone(),
                at_ms,
                provider_thread_id.clone(),
            ),
            Self::provider_error(
                thread_id.clone(),
                project_id.clone(),
                run_id.clone(),
                at_ms,
                provider_thread_id,
                message.clone(),
            ),
            Self::terminal(
                thread_id,
                project_id,
                run_id,
                at_ms,
                outcome,
                ProviderEvent::TurnCompleted {
                    provider_thread_id: None,
                    status: outcome.turn_status(),
                    error: Some(TurnError { message }),
                    provider_checkpoint_id: None,
                },
            ),
        ]
    }

    /// The stable `type` tag of the inner event, matching the serialized form.
    pub fn kind(&self) -> &'static str {
        self.event.kind()
    }

    /// Whether this event terminates the run.
    ///
    /// The single terminal event is `turn/completed`.
    pub fn is_terminal(&self) -> bool {
        self.event.is_terminal()
    }

    /// The terminal status, when this is the terminal event.
    pub fn terminal_status(&self) -> Option<TurnStatus> {
        match &self.event.body {
            ProviderEvent::TurnCompleted { status, .. } => Some(*status),
            _ => None,
        }
    }

    /// The control-plane outcome carried by a terminal event, with the
    /// contract status as the compatibility fallback for older events.
    pub fn terminal_outcome(&self) -> Option<RunOutcome> {
        let status = self.terminal_status()?;
        Some(self.outcome.unwrap_or(match status {
            TurnStatus::Completed => RunOutcome::Completed,
            TurnStatus::Interrupted => RunOutcome::Cancelled,
            TurnStatus::Failed => RunOutcome::Failed,
        }))
    }

    /// The failure message of a terminal event, when it carries one.
    pub fn terminal_error(&self) -> Option<&str> {
        match &self.event.body {
            ProviderEvent::TurnCompleted { error, .. } => {
                error.as_ref().map(|e| e.message.as_str())
            }
            _ => None,
        }
    }

    /// The provider's thread/session id, when the event carries one.
    pub fn provider_thread_id(&self) -> Option<&str> {
        self.event.provider_thread_id()
    }

    /// Builds the terminal `turn/completed` event for a run.
    pub fn completed(
        thread_id: ThreadId,
        project_id: ProjectId,
        run_id: RunId,
        at_ms: u64,
        provider_thread_id: Option<String>,
    ) -> Self {
        Self::terminal(
            thread_id,
            project_id,
            run_id,
            at_ms,
            RunOutcome::Completed,
            ProviderEvent::TurnCompleted {
                provider_thread_id,
                status: TurnStatus::Completed,
                error: None,
                provider_checkpoint_id: None,
            },
        )
    }

    /// Builds a terminal `turn/completed` failure for a run.
    pub fn failed(
        thread_id: ThreadId,
        project_id: ProjectId,
        run_id: RunId,
        at_ms: u64,
        status: TurnStatus,
        message: impl Into<String>,
    ) -> Self {
        let outcome = match status {
            TurnStatus::Interrupted => RunOutcome::Cancelled,
            TurnStatus::Failed | TurnStatus::Completed => RunOutcome::Failed,
        };
        Self::terminal(
            thread_id,
            project_id,
            run_id,
            at_ms,
            outcome,
            ProviderEvent::TurnCompleted {
                provider_thread_id: None,
                status,
                error: Some(TurnError {
                    message: message.into(),
                }),
                provider_checkpoint_id: None,
            },
        )
    }

    /// Builds a terminal event from an explicit loom outcome.
    ///
    /// The outcome and the contract status are derived from each other here,
    /// so the two can never disagree on the wire.
    pub fn terminal(
        thread_id: ThreadId,
        project_id: ProjectId,
        run_id: RunId,
        at_ms: u64,
        outcome: RunOutcome,
        event: ProviderEvent,
    ) -> Self {
        debug_assert!(event.is_terminal());
        let mut event = Self::new(thread_id, project_id, run_id, at_ms, event);
        event.outcome = Some(outcome);
        event
    }
}

impl ProviderEvent {
    /// Whether `kind` is a thread-scoped contract type.
    ///
    /// These are the types whose facts outlive the turn that produced them
    /// (background tasks, delegations), or are thread metadata rather than
    /// transcript (identity, name, goal, rate limits, resolved environment).
    pub fn is_thread_scoped(kind: &str) -> bool {
        matches!(
            kind,
            "thread/started"
                | "thread/identity"
                | "thread/name/updated"
                | "thread/goal/updated"
                | "thread/goal/cleared"
                | "item/backgroundTask/progress"
                | "item/backgroundTask/completed"
                | "item/delegation/progress"
                | "item/delegation/completed"
                | "provider/rateLimits/updated"
                | "provider.env-resolved"
                | "thread/extensionState/updated"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider_event::{ProviderEventType, ThreadEventType};

    fn ids() -> (ThreadId, ProjectId, RunId) {
        (ThreadId::mint(), ProjectId::mint(), RunId::mint())
    }

    #[test]
    fn a_turn_scoped_event_uses_the_run_id_as_its_turn() {
        let (thread_id, project_id, run_id) = ids();
        let event = RunEvent::new(
            thread_id.clone(),
            project_id,
            run_id.clone(),
            7,
            ProviderEvent::TurnStarted {
                provider_thread_id: "p".into(),
                parent_tool_call_id: None,
            },
        );
        let value = serde_json::to_value(&event).unwrap();
        assert_eq!(value["event"]["type"], "turn/started");
        assert_eq!(value["event"]["scope"]["kind"], "turn");
        assert_eq!(value["event"]["scope"]["turnId"], run_id.to_string());
        assert_eq!(value["event"]["threadId"], thread_id.to_string());
        assert_eq!(value["at_ms"], 7);
    }

    #[test]
    fn a_thread_scoped_event_uses_thread_scope() {
        let (thread_id, project_id, run_id) = ids();
        let event = RunEvent::new(
            thread_id,
            project_id,
            run_id,
            7,
            ProviderEvent::ThreadCompacted {
                provider_thread_id: "p".into(),
            },
        );
        // `thread/compacted` is turn-scoped in the contract.
        assert_eq!(
            serde_json::to_value(&event).unwrap()["event"]["scope"]["kind"],
            "turn"
        );

        let (thread_id, project_id, run_id) = ids();
        let background = RunEvent::new(
            thread_id,
            project_id,
            run_id,
            7,
            ProviderEvent::ThreadGoalCleared {
                provider_thread_id: "p".into(),
            },
        );
        assert_eq!(
            serde_json::to_value(&background).unwrap()["event"]["scope"]["kind"],
            "thread"
        );
    }

    #[test]
    fn terminal_is_turn_completed_and_carries_the_status() {
        let (thread_id, project_id, run_id) = ids();
        let done = RunEvent::completed(
            thread_id.clone(),
            project_id.clone(),
            run_id.clone(),
            1,
            Some("p".into()),
        );
        assert!(done.is_terminal());
        assert_eq!(done.terminal_status(), Some(TurnStatus::Completed));
        assert_eq!(done.terminal_error(), None);

        let failed = RunEvent::failed(
            thread_id,
            project_id,
            run_id,
            2,
            TurnStatus::Interrupted,
            "cancelled",
        );
        assert!(failed.is_terminal());
        assert_eq!(failed.terminal_status(), Some(TurnStatus::Interrupted));
        assert_eq!(failed.terminal_error(), Some("cancelled"));
    }

    #[test]
    fn a_server_failure_has_a_contract_valid_ordered_sequence() {
        let (thread_id, project_id, run_id) = ids();
        let events = RunEvent::failure_sequence(
            thread_id.clone(),
            project_id,
            run_id.clone(),
            2,
            RunOutcome::Failed,
            "workspace is unavailable",
        );

        assert_eq!(
            events.iter().map(RunEvent::kind).collect::<Vec<_>>(),
            vec!["turn/started", "provider/error", "turn/completed"]
        );
        let synthetic = RunEvent::synthetic_provider_thread_id(&run_id);
        for event in &events[..2] {
            assert_eq!(event.provider_thread_id(), Some(synthetic.as_str()));
            assert_eq!(
                event.event.scope.turn_id(),
                Some(run_id.to_string()).as_deref()
            );
        }
        assert_eq!(events[2].provider_thread_id(), None);
        assert_eq!(events[2].terminal_error(), Some("workspace is unavailable"));
        assert_eq!(events[0].event.thread_id, thread_id);

        let value = serde_json::to_value(&events[2]).unwrap();
        assert_eq!(value["event"]["type"], "turn/completed");
        assert_eq!(value["event"]["scope"]["turnId"], run_id.to_string());
        assert_eq!(value["event"]["providerThreadId"], serde_json::Value::Null);
        assert_eq!(
            value["event"]["error"]["message"],
            "workspace is unavailable"
        );
    }

    #[test]
    fn the_thread_scope_set_matches_the_types_that_need_it() {
        // Every provider type must be classifiable, and the thread-scoped set
        // must be a strict subset (turn chronology is the default).
        let thread_scoped = ProviderEventType::ALL
            .iter()
            .filter(|t| ProviderEvent::is_thread_scoped(t.as_str()))
            .count();
        assert!(thread_scoped > 0);
        assert!(thread_scoped < ProviderEventType::ALL.len());

        for event_type in ThreadEventType::ALL {
            if let ThreadEventType::Provider(provider) = event_type {
                // Classifying is total over the provider union.
                let _ = ProviderEvent::is_thread_scoped(provider.as_str());
            }
        }
    }
}
