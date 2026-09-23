//! Shared provider-run metadata and terminal-event construction.
//!
//! Provider execution is ACP-only. The ACP driver lives in [`crate::acp`]; this
//! module contains the run envelope shared by that driver and the worker's
//! failure/reconciliation path. Keeping the terminal constructor here means a
//! failure before an ACP session has an identity still produces the same
//! contract event as a failure after one has been opened.

use std::time::Duration;

use loom_domain::{
    automation::PermissionMode, HostPermissionMode, ProviderEvent, ReasoningLevel, RunEvent,
    RunOutcome, TurnError,
};
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
    /// How long the run may stay *silent* before the worker kills it.
    ///
    /// The bound is on silence, not on the turn: every event the run reports
    /// re-arms it, and an item that started and has not completed holds it off,
    /// so a long tool is never mistaken for a wedged agent — the same rule the
    /// embedded adapter's own settle fallback uses. `0` removes it and leaves
    /// [`ProviderRun::ceiling`] as the only bound.
    pub timeout: Duration,
    /// How long the run may take in total, however active it is.
    ///
    /// The last-resort bound. A run that keeps reporting is not stuck, so this
    /// is the only thing that ends an agent wedged with a tool call still open —
    /// the case [`ProviderRun::timeout`] deliberately holds off for. `0`
    /// removes it.
    pub ceiling: Duration,
    /// How long an agent's permission request waits for a user before it is
    /// cancelled.
    ///
    /// Tighter than [`ProviderRun::timeout`] on purpose: a run blocked with no
    /// client attached is a question nobody will answer, and holding it until
    /// the run deadline would leave the thread `working` far longer than the
    /// question deserves. Cancellation is the outcome, never an approval — see
    /// `crate::acp::permission`.
    pub permission_timeout: Duration,
    /// How long an accepted turn may be *silent* before the embedded `pi-acp`
    /// gives up on it through its own settle fallback.
    ///
    /// Distinct from [`ProviderRun::timeout`], which bounds the run's silence
    /// from the worker's side: this one is the adapter's own fallback and only
    /// runs when `pi-acp` is embedded. Every event the agent sends re-arms it,
    /// and a tool the agent is running holds it off, so a long build, a long
    /// download or a long streamed answer is never mistaken for a stuck agent.
    /// `0` disables it and leaves `timeout` as the only bound.
    pub settle_timeout: Duration,
    /// The host's maximum permission policy for this run.
    pub permission_ceiling: HostPermissionMode,
    /// The run's requested permission policy after applying the host ceiling.
    pub permission_mode: PermissionMode,
    /// The agent's identifier for this thread's conversation, when a previous
    /// run already opened one.
    pub provider_session_id: Option<String>,
    /// The model the client chose for the thread, in the agent's own terms.
    ///
    /// Applied through the session's `model` config option; a value the agent
    /// no longer offers is refused by the agent, not by this side.
    pub model: Option<String>,
    /// How much reasoning the client asked for, in loom's own terms.
    ///
    /// Mapped onto the levels the agent advertises for *the model it holds*,
    /// so a level the chosen model does not offer is left unset.
    pub reasoning_level: Option<ReasoningLevel>,
}

impl ProviderRun {
    /// Builds a run from a dispatch and the worker's local overrides.
    pub fn from_dispatch(
        dispatch: &RunDispatch,
        spec: ProviderSpec,
        timeout: Duration,
        ceiling: Duration,
        permission_timeout: Duration,
        settle_timeout: Duration,
    ) -> Self {
        Self {
            spec,
            prompt: dispatch.prompt.clone(),
            host_id: dispatch.host_id.clone(),
            thread_id: dispatch.thread_id.clone(),
            project_id: dispatch.project_id.clone(),
            run_id: dispatch.run_id.clone(),
            timeout,
            ceiling,
            permission_timeout,
            settle_timeout,
            permission_ceiling: dispatch.permission_ceiling,
            permission_mode: dispatch.permission_ceiling.clamp(dispatch.permission_mode),
            provider_session_id: dispatch.provider_session_id.clone(),
            model: dispatch.model.clone(),
            reasoning_level: dispatch.reasoning_level.clone(),
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
