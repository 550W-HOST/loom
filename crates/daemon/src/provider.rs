//! Shared provider-run metadata and terminal-event construction.
//!
//! Provider execution is ACP-only. The ACP driver lives in [`crate::acp`]; this
//! module contains the run envelope shared by that driver and the daemon's
//! failure/reconciliation path. Keeping the terminal constructor here means a
//! failure before an ACP session has an identity still produces the same
//! contract event as a failure after one has been opened.

use std::time::Duration;

use loom_domain::{ProviderEvent, RunEvent, RunOutcome, TurnError};
use loom_provider_protocol::{ProviderSpec, RunDispatch};

/// Everything one ACP run needs to report events.
#[derive(Clone, Debug)]
pub struct ProviderRun {
    /// How to reach the ACP agent.
    pub spec: ProviderSpec,
    /// The user turn that started the run.
    pub prompt: String,
    /// The host executing it, echoed in every report.
    pub host_id: loom_domain::HostId,
    /// The thread being advanced.
    pub thread_id: loom_domain::ThreadId,
    /// Its project.
    pub project_id: loom_domain::ProjectId,
    /// The run's identity.
    pub run_id: loom_domain::RunId,
    /// Daemon-side deadline. On expiry the ACP session is terminated and the
    /// run is reported as timed out.
    pub timeout: Duration,
    /// The agent's identifier for this thread's conversation, when a previous
    /// run already opened one.
    pub provider_session_id: Option<String>,
}

impl ProviderRun {
    /// Builds a run from a dispatch and the daemon's local overrides.
    pub fn from_dispatch(dispatch: &RunDispatch, spec: ProviderSpec, timeout: Duration) -> Self {
        Self {
            spec,
            prompt: dispatch.prompt.clone(),
            host_id: dispatch.host_id.clone(),
            thread_id: dispatch.thread_id.clone(),
            project_id: dispatch.project_id.clone(),
            run_id: dispatch.run_id.clone(),
            timeout,
            provider_session_id: dispatch.provider_session_id.clone(),
        }
    }

    /// Uses the known ACP session id, or the loom thread id for a failure that
    /// happened before the agent could name a session.
    pub fn provider_thread_id(&self) -> String {
        self.provider_session_id
            .clone()
            .unwrap_or_else(|| self.thread_id.to_string())
    }
}

/// Builds the one terminal event used when the ACP path fails before it emits
/// its own terminal event.
pub fn terminal_event(run: &ProviderRun, outcome: RunOutcome, message: &str) -> RunEvent {
    let body = ProviderEvent::TurnCompleted {
        provider_thread_id: Some(run.provider_thread_id()),
        status: outcome.turn_status(),
        error: match outcome {
            RunOutcome::Completed => None,
            _ => Some(TurnError {
                message: message.to_owned(),
            }),
        },
        provider_checkpoint_id: None,
    };
    RunEvent::terminal(
        run.thread_id.clone(),
        run.project_id.clone(),
        run.run_id.clone(),
        now_ms(),
        outcome,
        body,
    )
}

fn now_ms() -> u64 {
    loom_relay::now_ms()
}
