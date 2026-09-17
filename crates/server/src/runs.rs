//! In-flight provider runs: dispatch, reporting, and reconciliation.
//!
//! This is the control plane's half of the provider contract in
//! [`loom_provider_protocol`]. Three responsibilities, and the reason each is
//! here rather than in a route handler:
//!
//! 1. **Dispatch goes through the relay, to the environment's host.**
//!    [`AppState::dispatch_thread`] resolves the thread's environment, fills the
//!    provider's working directory from it, mints a run, and publishes a
//!    [`RunDispatch`] to that host's scope. The handler never touches a daemon
//!    socket, so once the run is in the log a reconnect cannot lose it. A thread
//!    with no usable environment, or whose host is detached, is *failed*, never
//!    run somewhere else.
//! 2. **Reports become thread events.** [`AppState::apply_run_report`] turns a
//!    daemon observation into a `thread_run_event` and publishes it to the
//!    thread scope, in order. The daemon's socket is not the fan-out path.
//! 3. **Every run reaches a terminal state.** [`AppState::reconcile_runs`] is
//!    the backstop: a provider that never reports, a daemon that stops
//!    heartbeating, and a deadline that passed all end in exactly one terminal
//!    `turn/completed` event, which moves the thread out of `working`.
//!
//! Point 3 is the one that cannot be left to the execution plane. A provider
//! crash, a killed daemon and a network partition are indistinguishable from
//! the control plane's point of view, and in all three cases the thread must
//! stop saying `working`. Making the *server* own the deadline and the
//! stale-heartbeat sweep is what makes that guarantee independent of the bug
//! that caused the failure.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};

use loom_domain::AutomationRunOutcome;
use loom_domain::{
    DomainEvent, Environment, EnvironmentStatus, HostId, HostStatus, ProjectId, ProviderEvent,
    RunEvent, RunId, RunOutcome, Thread, ThreadId, ThreadStatus, ThreadTrigger, TurnError,
};
use loom_provider_protocol::{ProviderReport, RunDispatch};
use loom_relay::{now_ms, Result as RelayResult, Scope};
use serde::{Deserialize, Serialize};

use crate::state::AppState;

/// A provider run or preflight attempt that has not reached a terminal event.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunRecord {
    /// The run's identity, also the idempotency key for dispatch delivery.
    pub run_id: RunId,
    /// The thread being advanced.
    pub thread_id: ThreadId,
    /// Its project.
    pub project_id: ProjectId,
    /// The host that owns the run. Only this host's reports are accepted.
    pub host_id: HostId,
    /// The workspace the provider was dispatched into.
    ///
    /// Recorded because the provider session id a run reports belongs to this
    /// directory; learning the binding from the environment later would describe
    /// a re-bound environment, not the run that opened the session. See
    /// [`AppState::learn_provider_session`].
    pub cwd: String,
    /// When the dispatch was published.
    pub started_at_ms: u64,
    /// When the run must be terminal, or the server reaps it.
    pub deadline_ms: u64,
    /// Whether a contract `turn/started` event has been published.
    ///
    /// A daemon can disappear before it reports its first event. The server
    /// uses this bit to add exactly one synthetic start before a terminal
    /// event, while preserving a real start when one was already observed.
    #[serde(default)]
    pub turn_started: bool,
    /// The provider identity carried by the run's start, when known.
    ///
    /// A synthetic value is kept here only as run bookkeeping. It is never
    /// copied into `Thread::provider_session_id` and can therefore never be
    /// used to resume an ACP session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_thread_id: Option<String>,
    /// Whether a provider error has already been published for this run.
    ///
    /// This avoids adding a duplicate diagnostic while recovering a run whose
    /// provider error was already in the log before the server restarted.
    #[serde(default)]
    pub provider_error_reported: bool,
    /// The server-owned failure reason, when the attempt failed before a
    /// provider could be dispatched. Keeping it on the record lets a retry or
    /// restart finish the same lifecycle without replacing the root cause.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_reason: Option<String>,
    /// Whether the terminal run event has been published.
    #[serde(default)]
    pub terminal_published: bool,
    /// The outcome of the published terminal event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_outcome: Option<RunOutcome>,
    /// The thread status event still waiting to be published, if any.
    ///
    /// The status mutation is applied only after this event reaches the relay,
    /// so a failed append leaves the thread in `working` and can be retried.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_status_event: Option<PendingStatusChange>,
}

/// The durable data needed to retry one terminal thread-status append.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingStatusChange {
    /// The thread whose status changes.
    pub thread_id: ThreadId,
    /// The project containing the thread.
    pub project_id: ProjectId,
    /// The status before the transition.
    pub from: ThreadStatus,
    /// The status after the transition.
    pub to: ThreadStatus,
    /// The original transition timestamp.
    pub at_ms: u64,
}

impl PendingStatusChange {
    /// Builds the domain event that is stored in the relay.
    fn event(&self) -> DomainEvent {
        DomainEvent::ThreadStatusChanged {
            thread_id: self.thread_id.clone(),
            project_id: self.project_id.clone(),
            from: self.from,
            to: self.to,
            at_ms: self.at_ms,
        }
    }

    /// Whether a replayed event is the pending append.
    pub(crate) fn matches_event(&self, event: &DomainEvent) -> bool {
        matches!(
            event,
            DomainEvent::ThreadStatusChanged {
                thread_id,
                project_id,
                from,
                to,
                at_ms,
            } if thread_id == &self.thread_id
                && project_id == &self.project_id
                && from == &self.from
                && to == &self.to
                && at_ms == &self.at_ms
        )
    }
}

/// Runs currently in flight, keyed by run id.
///
/// In-memory during normal operation, and included in the durable domain
/// snapshot when the disk relay backend is enabled. A restarted server restores
/// these records only long enough to close them with a contract-valid terminal
/// sequence; a provider report arriving after that is an idempotent no-op.
#[derive(Debug, Default)]
pub struct RunRegistry {
    /// Serializes lifecycle publication with dispatch and reconciliation. The
    /// relay append is synchronous, so holding this lock makes a start and a
    /// terminal event one indivisible state-machine step to other callers.
    lifecycle: Mutex<()>,
    inner: Mutex<RunRegistryState>,
}

#[derive(Debug, Default)]
struct RunRegistryState {
    records: HashMap<RunId, RunRecord>,
    /// Claims cover both records and the short preflight window before a
    /// record can be inserted. This makes concurrent dispatches for one
    /// thread choose one run deterministically.
    thread_claims: HashMap<ThreadId, RunId>,
}

impl RunRegistry {
    /// An empty table.
    pub fn new() -> Self {
        Self::default()
    }

    /// Locks run lifecycle transitions and event publication.
    pub(crate) fn lifecycle_lock(&self) -> MutexGuard<'_, ()> {
        self.lifecycle
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    /// Records a run. Returns the previous record if the id was reused, which
    /// cannot happen for minted ids but keeps the API total.
    pub fn insert(&self, record: RunRecord) -> Option<RunRecord> {
        let mut state = self.lock();
        let previous = state.records.insert(record.run_id.clone(), record.clone());
        if let Some(previous) = &previous {
            if state.thread_claims.get(&previous.thread_id) == Some(&previous.run_id) {
                state.thread_claims.remove(&previous.thread_id);
            }
        }
        state
            .thread_claims
            .insert(record.thread_id.clone(), record.run_id.clone());
        previous
    }

    /// Restores runs from a domain snapshot before recovery settles them.
    pub(crate) fn restore(&self, records: impl IntoIterator<Item = RunRecord>) {
        for record in records {
            self.insert(record);
        }
    }

    /// Claims a thread before a dispatch is resolved or published.
    ///
    /// The claim is also used by pre-dispatch failures, which have no
    /// `RunRecord` because there is no provider run to reconcile.
    pub fn claim_thread(&self, thread_id: &ThreadId, run_id: RunId) -> Result<(), RunId> {
        let mut state = self.lock();
        if let Some(existing) = state.thread_claims.get(thread_id) {
            return Err(existing.clone());
        }
        state.thread_claims.insert(thread_id.clone(), run_id);
        Ok(())
    }

    /// Releases a preflight claim when its terminal sequence has been emitted.
    pub fn release_thread(&self, thread_id: &ThreadId, run_id: &RunId) -> bool {
        let mut state = self.lock();
        if state.thread_claims.get(thread_id) != Some(run_id) {
            return false;
        }
        state.thread_claims.remove(thread_id);
        true
    }

    /// A run by id.
    pub fn get(&self, run_id: &RunId) -> Option<RunRecord> {
        self.lock().records.get(run_id).cloned()
    }

    /// Removes a run, returning it if it was in flight.
    pub fn remove(&self, run_id: &RunId) -> Option<RunRecord> {
        let mut state = self.lock();
        let record = state.records.remove(run_id)?;
        if state.thread_claims.get(&record.thread_id) == Some(run_id) {
            state.thread_claims.remove(&record.thread_id);
        }
        Some(record)
    }

    /// Marks the first published turn event and records its provider identity.
    ///
    /// Returns `false` when the run is gone or was already marked. Callers do
    /// this only after the event append succeeds so a failed append never
    /// suppresses the synthetic start needed by a later terminal path.
    pub fn mark_started(&self, run_id: &RunId, provider_thread_id: String) -> bool {
        let mut state = self.lock();
        let Some(record) = state.records.get_mut(run_id) else {
            return false;
        };
        if record.turn_started {
            return false;
        }
        record.turn_started = true;
        record.provider_thread_id = Some(provider_thread_id);
        true
    }

    /// Records that a provider error has been published for a run.
    pub fn mark_provider_error(&self, run_id: &RunId) {
        if let Some(record) = self.lock().records.get_mut(run_id) {
            record.provider_error_reported = true;
        }
    }

    /// Records that the terminal run event reached the relay.
    pub fn mark_terminal(&self, run_id: &RunId, outcome: RunOutcome) -> bool {
        let mut state = self.lock();
        let Some(record) = state.records.get_mut(run_id) else {
            return false;
        };
        record.terminal_published = true;
        record.terminal_outcome = Some(outcome);
        true
    }

    /// Keeps the status event needed to finish a terminal run.
    pub fn set_pending_status_event(&self, run_id: &RunId, event: PendingStatusChange) -> bool {
        let mut state = self.lock();
        let Some(record) = state.records.get_mut(run_id) else {
            return false;
        };
        record.pending_status_event = Some(event);
        true
    }

    /// Clears a status event after its relay append succeeds.
    pub fn clear_pending_status_event(&self, run_id: &RunId) -> bool {
        let mut state = self.lock();
        let Some(record) = state.records.get_mut(run_id) else {
            return false;
        };
        record.pending_status_event = None;
        true
    }

    /// Every in-flight run on one host.
    pub fn for_host(&self, host_id: &HostId) -> Vec<RunRecord> {
        let mut runs: Vec<RunRecord> = self
            .lock()
            .records
            .values()
            .filter(|run| &run.host_id == host_id)
            .cloned()
            .collect();
        runs.sort_by(|left, right| left.run_id.cmp(&right.run_id));
        runs
    }

    /// Every in-flight run.
    pub fn all(&self) -> Vec<RunRecord> {
        let mut runs: Vec<RunRecord> = self.lock().records.values().cloned().collect();
        runs.sort_by(|left, right| left.run_id.cmp(&right.run_id));
        runs
    }

    /// The in-flight run advancing one thread, when there is one.
    ///
    /// A thread has at most one run in flight — dispatch is only reached from a
    /// status transition — so this is a single lookup, not a list. The lowest
    /// run id wins if that invariant is ever broken, which keeps the answer
    /// deterministic instead of arbitrary.
    pub fn for_thread(&self, thread_id: &ThreadId) -> Option<RunRecord> {
        let mut runs: Vec<RunRecord> = self
            .lock()
            .records
            .values()
            .filter(|run| &run.thread_id == thread_id)
            .cloned()
            .collect();
        runs.sort_by(|left, right| left.run_id.cmp(&right.run_id));
        runs.into_iter().next()
    }

    /// In-flight runs whose deadline has passed at `now_ms`.
    pub fn expired(&self, now_ms: u64) -> Vec<RunRecord> {
        let mut runs: Vec<RunRecord> = self
            .lock()
            .records
            .values()
            .filter(|run| run.deadline_ms <= now_ms)
            .cloned()
            .collect();
        runs.sort_by(|left, right| left.run_id.cmp(&right.run_id));
        runs
    }

    /// How many runs are in flight.
    pub fn len(&self) -> usize {
        self.lock().records.len()
    }

    /// Whether no run is in flight.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn lock(&self) -> MutexGuard<'_, RunRegistryState> {
        self.inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }
}

/// What happened when a dispatch was attempted.
#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(clippy::large_enum_variant)]
pub enum DispatchOutcome {
    /// The dispatch was appended to the host's scope and the run is in flight.
    Dispatched(RunRecord),
    /// Another dispatch already owns this thread.
    AlreadyInFlight {
        /// The run currently claiming the thread.
        run_id: RunId,
    },
    /// The thread has no usable environment, so there is no workspace to run
    /// the provider in. The run was failed on the spot rather than letting a
    /// provider start in the daemon's own cwd.
    NoEnvironment {
        /// The synthetic run id reported in the terminal event.
        run_id: RunId,
        /// Why the thread could not run: the same reason the terminal event
        /// carries, so a caller that has to report it (an automation run) does
        /// not have to guess from the thread's timeline.
        reason: String,
    },
    /// No execution machine is connected. The run was failed on the spot so the
    /// thread does not sit in `working` waiting for a machine that is not
    /// there.
    NoHost {
        /// The synthetic run id reported in the terminal event.
        run_id: RunId,
        /// Why the thread could not run, as in [`DispatchOutcome::NoEnvironment`].
        reason: String,
    },
    /// The relay rejected the append; the run was failed on the spot.
    PublishFailed {
        /// The run or preflight attempt whose lifecycle could not be published.
        run_id: RunId,
        /// Why the append failed.
        error: String,
    },
}

/// Whether a report was applied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReportOutcome {
    /// The event was published (and, for a terminal event, the thread moved).
    Applied,
    /// The run is not in flight. Normal under redelivery: the daemon may
    /// report a terminal event twice after a reconnect.
    Unknown,
    /// The report named a run this host does not own, or a different thread.
    Mismatch(String),
    /// The report was valid, but the relay rejected its append. The run stays
    /// in flight when the failed append was non-terminal, so a retry can make
    /// progress after the backend recovers.
    PublishFailed {
        /// Why the relay rejected the event.
        error: String,
    },
}

/// The result of claiming a run's terminal transition.
#[derive(Debug, PartialEq, Eq)]
enum FinishRunResult {
    /// The run was claimed and its lifecycle was settled.
    Finished,
    /// Another caller already claimed the terminal transition.
    AlreadyFinished,
    /// The terminal sequence could not be appended. The run remains in flight
    /// with progress flags advanced for events that did append, so a retry can
    /// finish the same run without duplicating those events.
    PublishFailed(String),
}

/// What a reconciliation pass found and repaired.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReconcileSummary {
    /// Hosts marked disconnected for missing their heartbeat window.
    pub stale_hosts: usize,
    /// Runs failed because their host went stale.
    pub stale_runs: usize,
    /// Runs failed because their deadline passed.
    pub timed_out_runs: usize,
    /// Queued messages that became due and were delivered.
    pub sent_queued_messages: usize,
}

/// What a `threads.stop` request found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StopOutcome {
    /// A run was in flight and has been terminated.
    Stopped,
    /// The thread had no run in flight; there was nothing to terminate.
    NoRun,
    /// The terminal lifecycle could not be published, so the run remains in
    /// flight and a later stop or reconciliation can retry it.
    PublishFailed {
        /// Why the relay rejected the lifecycle event.
        error: String,
    },
}

impl AppState {
    /// Mints a run, records it, and publishes the dispatch to `host:{id}`.
    ///
    /// The caller must already have moved `thread` into `working` (a user
    /// message does that). This method only decides *where* it runs, records
    /// that it is running, and — the point of the whole path — tells the daemon
    /// **which directory** to run the provider in.
    ///
    /// The workspace comes from the thread's environment: its path fills
    /// [`ProviderSpec::cwd`] and its host is the machine the dispatch targets,
    /// because the directory only exists there. A thread with no environment,
    /// an environment that is not `ready`, or a host that is detached is failed
    /// on the spot rather than silently run somewhere else. See
    /// [`AppState::dispatch_thread`] and `docs/provider-protocol.md`.
    pub fn dispatch_thread(&self, thread: &Thread, prompt: &str) -> DispatchOutcome {
        let _lifecycle = self.runs.lifecycle_lock();
        let now = now_ms();
        let run_id = RunId::mint();
        if let Err(existing) = self.runs.claim_thread(&thread.id, run_id.clone()) {
            return DispatchOutcome::AlreadyInFlight { run_id: existing };
        }
        // Keep the entity view aligned with the claim while the dispatcher is
        // resolving the environment. A preflight failure clears it below.
        let _ = self.registry.set_thread_run(&thread.id, &run_id, now);

        let environment = match self.resolve_environment(thread) {
            Ok(environment) => environment,
            Err(reason) => {
                return match self.fail_thread(thread, run_id.clone(), reason.clone(), now) {
                    Ok(()) => DispatchOutcome::NoEnvironment { run_id, reason },
                    Err(error) => DispatchOutcome::PublishFailed { run_id, error },
                };
            }
        };

        // The workspace lives on exactly one machine, so the run must go to the
        // environment's host rather than to whichever host is "primary".
        let Some(host) = self.registry.host(&environment.host_id) else {
            let reason = format!("host {} is not known", environment.host_id);
            return match self.fail_thread(thread, run_id.clone(), reason.clone(), now) {
                Ok(()) => DispatchOutcome::NoHost { run_id, reason },
                Err(error) => DispatchOutcome::PublishFailed { run_id, error },
            };
        };
        if host.status != HostStatus::Connected {
            let reason = format!(
                "host {} owns this thread's workspace but is not connected",
                host.id
            );
            return match self.fail_thread(thread, run_id.clone(), reason.clone(), now) {
                Ok(()) => DispatchOutcome::NoHost { run_id, reason },
                Err(error) => DispatchOutcome::PublishFailed { run_id, error },
            };
        }

        // A `ready` environment always has a path for a managed one and, by
        // construction, for an unmanaged one. Treat `None` as an internal
        // inconsistency rather than dispatch a provider without a workspace.
        let Some(workspace) = environment.path.clone() else {
            let reason = format!("environment {} has no workspace path", environment.id);
            return match self.fail_thread(thread, run_id.clone(), reason.clone(), now) {
                Ok(()) => DispatchOutcome::NoEnvironment { run_id, reason },
                Err(error) => DispatchOutcome::PublishFailed { run_id, error },
            };
        };

        let mut record = RunRecord {
            run_id: run_id.clone(),
            thread_id: thread.id.clone(),
            project_id: thread.project_id.clone(),
            host_id: host.id.clone(),
            cwd: workspace.clone(),
            started_at_ms: now,
            deadline_ms: now.saturating_add(self.run_timeout_ms()),
            turn_started: false,
            provider_thread_id: None,
            provider_error_reported: false,
            failure_reason: None,
            terminal_published: false,
            terminal_outcome: None,
            pending_status_event: None,
        };
        // The dispatched spec carries the working directory. The provider is
        // otherwise exactly the one this server was configured with.
        let mut provider = self.provider_spec().clone();
        provider.cwd = Some(workspace.clone());
        // Resume only when the recorded session belongs to this agent *and*
        // this workspace. A provider session id is the agent's own, unique only
        // within it, and a session is bound to the directory it was opened in —
        // so resuming across either boundary would either hand an agent an id it
        // never issued or reopen a conversation about the wrong project. The
        // binding is what makes that check possible; a thread with none (an
        // older snapshot, or a session established by another agent) starts
        // fresh, which is recoverable where a wrong resume is not.
        let provider_session_id = thread
            .resumable_session_id(&provider.name, &workspace)
            .map(str::to_owned);
        // This is only the identity the provider may resume. If no session is
        // available, the run gets a synthetic identity only if it later needs
        // a server-generated start event.
        record.provider_thread_id = provider_session_id.clone();
        let dispatch = RunDispatch {
            run_id: run_id.clone(),
            thread_id: thread.id.clone(),
            // The thread already knows the agent's conversation id once a
            // first run has reported it, so a later turn continues that
            // conversation rather than starting over. `None` means this is
            // the first run, or that the recorded one belongs elsewhere.
            provider_session_id,
            project_id: thread.project_id.clone(),
            host_id: host.id.clone(),
            prompt: prompt.to_owned(),
            provider,
            permission_ceiling: host.max_permission_mode,
            deadline_ms: record.deadline_ms,
            created_at_ms: now,
        };
        let payload =
            serde_json::to_vec(&dispatch).expect("a RunDispatch always serializes to JSON");

        self.runs.insert(record.clone());
        let _ = self.registry.set_thread_run(&thread.id, &run_id, now);

        match self.publish(Scope::Host(host.id.to_string()), payload) {
            Ok(_) => DispatchOutcome::Dispatched(record),
            Err(error) => {
                // The run never reached the log, so the execution plane can
                // never report it. Fail it here rather than wait for the
                // deadline sweep.
                let error_text = error.to_string();
                let _ = self.finish_run_with_locked(
                    &record,
                    RunOutcome::Failed,
                    Some(error_text.clone()),
                    None,
                    now,
                );
                DispatchOutcome::PublishFailed {
                    run_id,
                    error: error_text,
                }
            }
        }
    }

    /// Terminates the run advancing a thread, if there is one.
    ///
    /// A stop goes through the same terminal path as every other outcome —
    /// [`AppState::finish_run`] with [`RunOutcome::Cancelled`]: exactly one
    /// `turn/completed` event with status `interrupted` reaches the thread
    /// scope, the run leaves the table, and the thread returns to `idle`.
    /// Inventing a second thread state machine for cancellation is the thing
    /// this deliberately does not do.
    ///
    /// The daemon is **not** told. The provider protocol has no cancel frame,
    /// so the provider process runs to its own end and its later reports are
    /// dropped as unknown runs — the same handling a superseded run already
    /// gets. Teaching the execution plane to abort is a provider-protocol
    /// change with its own issue; until then a stop is authoritative on the
    /// control plane and best-effort on the machine.
    pub fn stop_thread(&self, thread_id: &ThreadId) -> StopOutcome {
        let now = now_ms();
        let result = {
            let _lifecycle = self.runs.lifecycle_lock();
            let Some(record) = self.runs.for_thread(thread_id) else {
                return StopOutcome::NoRun;
            };
            self.finish_run_with_locked(
                &record,
                RunOutcome::Cancelled,
                Some("stopped by a client".to_owned()),
                None,
                now,
            )
        };
        match result {
            FinishRunResult::Finished => {
                self.drain_thread_queue(thread_id);
                StopOutcome::Stopped
            }
            FinishRunResult::AlreadyFinished => StopOutcome::NoRun,
            FinishRunResult::PublishFailed(error) => StopOutcome::PublishFailed { error },
        }
    }

    /// Resolves the environment a thread must run in.
    ///
    /// The explicit rejection of a thread with no environment is deliberate:
    /// falling back to the daemon's own cwd is the bug this path fixes, so an
    /// unbound thread is an error, never a silent default.
    fn resolve_environment(&self, thread: &Thread) -> Result<Environment, String> {
        let Some(environment_id) = &thread.environment_id else {
            return Err("thread has no environment bound; bind one before dispatching".to_owned());
        };
        let Some(environment) = self.registry.environment(environment_id) else {
            return Err(format!("environment {environment_id} is not known"));
        };
        if environment.status != EnvironmentStatus::Ready {
            return Err(format!(
                "environment {environment_id} is {} and not ready to run",
                environment.status
            ));
        }
        if environment.path.is_none() {
            return Err(format!(
                "environment {environment_id} has no workspace path"
            ));
        }
        Ok(environment)
    }

    /// Applies one daemon report.
    ///
    /// A report for an unknown run is dropped: that is what makes a daemon's
    /// post-reconnect redelivery idempotent. A report for a run this host does
    /// not own is rejected, so one machine cannot terminate another's turn.
    ///
    /// The event's own identity is checked against the run record as well, so a
    /// daemon cannot relabel one run's stream as another's.
    pub fn apply_run_report(&self, host_id: &HostId, report: ProviderReport) -> ReportOutcome {
        let now = now_ms();
        let run_id = report.event.run_id.clone();
        let lifecycle = self.runs.lifecycle_lock();
        let Some(record) = self.runs.get(&run_id) else {
            return ReportOutcome::Unknown;
        };
        if &record.host_id != host_id {
            return ReportOutcome::Mismatch(format!(
                "run {run_id} is owned by host {}, not {host_id}",
                record.host_id
            ));
        }
        if record.thread_id != report.event.thread_id {
            return ReportOutcome::Mismatch(format!(
                "run {run_id} belongs to thread {}, not {}",
                record.thread_id, report.event.thread_id
            ));
        }

        let event = report.event;
        if event.is_terminal() {
            // The daemon's own verdict travels in `outcome` when it has one;
            // for a terminal event a producer sent without it, the contract
            // status is the fallback.
            let outcome = event.terminal_outcome().unwrap_or(RunOutcome::Failed);
            let error = event.terminal_error().map(str::to_owned);
            let result = self.finish_run_with_locked(&record, outcome, error, Some(event), now);
            drop(lifecycle);
            return match result {
                FinishRunResult::Finished => {
                    self.drain_thread_queue(&record.thread_id);
                    ReportOutcome::Applied
                }
                FinishRunResult::AlreadyFinished => ReportOutcome::Unknown,
                FinishRunResult::PublishFailed(error) => ReportOutcome::PublishFailed { error },
            };
        } else {
            let event_kind = event.kind();
            if event_kind == "turn/started" {
                // A duplicate start is harmless but must not create a second
                // turn anchor in the projection.
                if record.turn_started {
                    return ReportOutcome::Applied;
                }
                let provider_thread_id = event
                    .provider_thread_id()
                    .map(str::to_owned)
                    .unwrap_or_else(|| RunEvent::synthetic_provider_thread_id(&run_id));
                if let Err(error) = self.publish_run_event(&record, event) {
                    return ReportOutcome::PublishFailed {
                        error: error.to_string(),
                    };
                }
                self.runs.mark_started(&run_id, provider_thread_id);
            } else {
                // Turn-scoped reports can arrive before the daemon's start
                // report after a reconnect. Add the anchor before forwarding
                // the report so the client projection remains legal.
                if !ProviderEvent::is_thread_scoped(event_kind) && !record.turn_started {
                    let provider_thread_id = event
                        .provider_thread_id()
                        .map(str::to_owned)
                        .or_else(|| record.provider_thread_id.clone())
                        .unwrap_or_else(|| RunEvent::synthetic_provider_thread_id(&run_id));
                    let started = RunEvent::started(
                        record.thread_id.clone(),
                        record.project_id.clone(),
                        record.run_id.clone(),
                        now,
                        provider_thread_id,
                    );
                    if let Err(error) = self.publish_run_event(&record, started) {
                        return ReportOutcome::PublishFailed {
                            error: error.to_string(),
                        };
                    }
                    let provider_thread_id = event
                        .provider_thread_id()
                        .map(str::to_owned)
                        .or_else(|| record.provider_thread_id.clone())
                        .unwrap_or_else(|| RunEvent::synthetic_provider_thread_id(&run_id));
                    self.runs.mark_started(&run_id, provider_thread_id);
                }
                let event_for_learning = event.clone();
                if let Err(error) = self.publish_run_event(&record, event) {
                    return ReportOutcome::PublishFailed {
                        error: error.to_string(),
                    };
                }
                if event_kind == "provider/error" {
                    self.runs.mark_provider_error(&run_id);
                }
                self.learn_provider_session(&event_for_learning, now);
            }
        }
        ReportOutcome::Applied
    }

    /// Records the agent's session id from a `thread/identity` event.
    ///
    /// This is how the control plane learns which conversation a thread is: the
    /// value is read from the event stream rather than requested over a second
    /// channel, so the adapter needs no callback and the dispatch path stays
    /// one-way. The stored id travels back on the next dispatch, which is what
    /// lets a later turn continue this conversation instead of starting over.
    ///
    /// The **binding** recorded with it is the other half: the agent that
    /// issued the id and the workspace it was opened in. Without it, a later run
    /// under a different agent or in a different workspace would be dispatched
    /// the id anyway, and only the agent could tell that it means nothing there.
    /// See [`loom_domain::Thread::resumable_session_id`].
    ///
    /// Idempotent: the agent reports its identity every run, and only a change
    /// produces an event.
    fn learn_provider_session(&self, event: &RunEvent, now: u64) {
        if event.kind() != "thread/identity" {
            return;
        }
        let Some(session_id) = event.provider_thread_id() else {
            return;
        };
        // The binding is read from the *in-flight run*, not the thread's
        // current environment: the environment can be re-bound between runs,
        // and the binding must describe the run that actually opened the
        // session. A run that is no longer in the table (a report after the
        // deadline reaped it) leaves the binding unknown, which starts the next
        // turn fresh rather than guessing.
        let binding = self.runs.get(&event.run_id).map(|record| {
            loom_domain::ProviderSessionBinding::new(
                self.provider_spec().name.clone(),
                record.cwd.clone(),
            )
            .at(now)
        });
        if let Some(learned) =
            self.registry
                .set_provider_session_id(&event.thread_id, session_id, binding, now)
        {
            // The thread's own record changed, so the sidebar sees it. Nothing
            // in the contract's thread shape carries this value; the event is
            // how a client could observe it.
            let _ = self.publish_domain_event(&learned);
        }
    }

    /// Reaps runs whose host went quiet or whose deadline passed.
    ///
    /// This is the guarantee that a thread cannot be stuck in `working`. It is
    /// deliberately driven from the server, because the failure it repairs is
    /// precisely "the execution plane is no longer able to tell us anything".
    pub fn reconcile_runs(&self, now: u64) -> ReconcileSummary {
        // Capabilities are process-local and intentionally not snapshotted, so
        // the same sweep that reaps runs also bounds their in-memory lifetime.
        self.join_codes.purge_expired(now);
        self.file_previews.purge_expired(now);
        // Interaction answers are a durable two-phase delivery. Retry them
        // before run deadlines so a transient relay failure cannot leave an
        // ACP permission request blocked until timeout.
        let _ = self.retry_resolving_interactions(now);
        let mut summary = ReconcileSummary::default();

        // 1. A host that has not heartbeat within the staleness window is
        //    detached, exactly as if its socket had closed.
        let stale_after = self.host_stale_after_ms();
        let mut stale_hosts = Vec::new();
        for host in self.registry.hosts() {
            if host.status != HostStatus::Connected {
                continue;
            }
            let seen = host.last_seen_at_ms.unwrap_or(host.updated_at_ms);
            if now.saturating_sub(seen) <= stale_after {
                continue;
            }
            if let Ok(events) = self.registry.mark_host_disconnected(&host.id, now) {
                for event in &events {
                    let _ = self.publish_domain_event(event);
                }
            }
            stale_hosts.push(host.id.clone());
        }

        // 2. Every in-flight run on a stale host is failed rather than left
        //    hanging on a machine that is gone.
        for host_id in &stale_hosts {
            summary.stale_hosts += 1;
            for record in self.runs.for_host(host_id) {
                if record.terminal_published {
                    continue;
                }
                if self.finish_run(
                    &record,
                    RunOutcome::HostStale,
                    Some(format!("host {host_id} stopped heartbeating")),
                    now,
                ) {
                    summary.stale_runs += 1;
                }
            }
        }

        // 3. A terminal event may have committed while its thread-status
        // append was rejected. Retry that second commit before considering
        // deadlines, so a transient relay failure cannot strand the record.
        for record in self.runs.all() {
            if !record.terminal_published {
                continue;
            }
            let outcome = record.terminal_outcome.unwrap_or(RunOutcome::Failed);
            let _ = self.finish_run_with(&record, outcome, None, None, now);
        }

        // 4. The server-side deadline is the backstop for a daemon that is
        //    connected but wedged. A run whose provider never settles is failed
        //    here even though nothing reported it.
        for record in self.runs.expired(now) {
            if self.runs.get(&record.run_id).is_none() {
                continue;
            }
            if record.terminal_published {
                continue;
            }
            let (outcome, error) = match &record.failure_reason {
                Some(reason) => (RunOutcome::Failed, Some(reason.clone())),
                None => (
                    RunOutcome::TimedOut,
                    Some("run exceeded its deadline".into()),
                ),
            };
            if self.finish_run(&record, outcome, error, now) && record.failure_reason.is_none() {
                summary.timed_out_runs += 1;
            }
        }

        // 5. Queued messages whose time has come. A scheduled message needs no
        //    other event to become due, so the sweep is what delivers it; a
        //    message left queued by a crash between a run's terminal event and
        //    its drain is picked up by the same pass.
        summary.sent_queued_messages = self.drain_due_queued_messages();

        summary
    }

    /// Publishes the server's own terminal event, clears the run, and moves
    /// the thread out of `working`.
    fn finish_run(
        &self,
        record: &RunRecord,
        outcome: RunOutcome,
        error: Option<String>,
        now: u64,
    ) -> bool {
        matches!(
            self.finish_run_with(record, outcome, error, None, now),
            FinishRunResult::Finished
        )
    }

    /// Publishes a terminal event, clears the run, and moves the thread out of
    /// `working`.
    ///
    /// `event` is the daemon's own terminal event when it reported one; for a
    /// server-reaped run (a deadline, a stale host) it is absent and the server
    /// synthesizes the contract event from the outcome.
    fn finish_run_with(
        &self,
        record: &RunRecord,
        outcome: RunOutcome,
        error: Option<String>,
        event: Option<RunEvent>,
        now: u64,
    ) -> FinishRunResult {
        let result = {
            let _lifecycle = self.runs.lifecycle_lock();
            self.finish_run_with_locked(record, outcome, error, event, now)
        };
        if matches!(result, FinishRunResult::Finished) {
            self.drain_thread_queue(&record.thread_id);
        }
        result
    }

    /// Publishes a terminal lifecycle while the registry lifecycle lock is
    /// held. The record is removed only after the terminal append succeeds, so
    /// a transient relay failure can be retried without losing the run.
    fn finish_run_with_locked(
        &self,
        record: &RunRecord,
        outcome: RunOutcome,
        error: Option<String>,
        event: Option<RunEvent>,
        now: u64,
    ) -> FinishRunResult {
        self.finish_run_with_locked_and_drain(record, outcome, error, event, now, false)
    }

    /// The recovery variant settles the thread without draining queued work.
    /// Recovery may have several stale runs to close, and dispatching a queued
    /// message between those closures would make the new run look stale too.
    pub(crate) fn fail_run_after_restart(&self, record: &RunRecord, now: u64) -> bool {
        let _lifecycle = self.runs.lifecycle_lock();
        let reason = record
            .failure_reason
            .clone()
            .unwrap_or_else(|| "server restarted while the run was in flight".to_owned());
        matches!(
            self.finish_run_with_locked_and_drain(
                record,
                RunOutcome::Failed,
                Some(reason),
                None,
                now,
                false,
            ),
            FinishRunResult::Finished
        )
    }

    /// Completes the in-memory cleanup for a terminal event that was already
    /// appended before the previous process stopped. Re-publishing that event
    /// would create a second terminal for the same run, so recovery only removes
    /// the record and applies the missing thread transition.
    pub(crate) fn recover_published_terminal(
        &self,
        record: &RunRecord,
        outcome: RunOutcome,
        now: u64,
    ) -> bool {
        let _lifecycle = self.runs.lifecycle_lock();
        let Some(record) = self.runs.get(&record.run_id) else {
            return false;
        };
        matches!(
            self.settle_finished_thread(&record, outcome, None, now, false),
            FinishRunResult::Finished
        )
    }

    fn finish_run_with_locked_and_drain(
        &self,
        record: &RunRecord,
        outcome: RunOutcome,
        error: Option<String>,
        event: Option<RunEvent>,
        now: u64,
        drain_queue: bool,
    ) -> FinishRunResult {
        let Some(record) = self.runs.get(&record.run_id) else {
            return FinishRunResult::AlreadyFinished;
        };
        if record.terminal_published {
            let outcome = record.terminal_outcome.unwrap_or(outcome);
            let error = error.or_else(|| record.failure_reason.clone());
            return self.settle_finished_thread(
                &record,
                outcome,
                error.as_deref(),
                now,
                drain_queue,
            );
        }
        let error = error.or_else(|| record.failure_reason.clone());

        // A daemon can report a terminal event before its start after a
        // reconnect. The start must be appended first, using the real provider
        // identity when the terminal carried one and a synthetic timeline-only
        // identity otherwise.
        let provider_thread_id = event
            .as_ref()
            .and_then(RunEvent::provider_thread_id)
            .map(str::to_owned)
            .or_else(|| record.provider_thread_id.clone())
            .unwrap_or_else(|| RunEvent::synthetic_provider_thread_id(&record.run_id));
        if !record.turn_started {
            let started = RunEvent::started(
                record.thread_id.clone(),
                record.project_id.clone(),
                record.run_id.clone(),
                now,
                provider_thread_id.clone(),
            );
            if let Err(error) = self.publish_run_event(&record, started) {
                return FinishRunResult::PublishFailed(error.to_string());
            }
            self.runs
                .mark_started(&record.run_id, provider_thread_id.clone());
        }

        // Failures owned by the control plane get a separate diagnostic row;
        // provider terminal reports already carry their own provider verdict.
        let server_failure = event.is_none()
            && matches!(
                outcome,
                RunOutcome::Failed | RunOutcome::TimedOut | RunOutcome::HostStale
            );
        if server_failure && !record.provider_error_reported {
            let diagnostic = RunEvent::provider_error(
                record.thread_id.clone(),
                record.project_id.clone(),
                record.run_id.clone(),
                now,
                provider_thread_id,
                error.clone().unwrap_or_default(),
            );
            if let Err(error) = self.publish_run_event(&record, diagnostic) {
                return FinishRunResult::PublishFailed(error.to_string());
            }
            self.runs.mark_provider_error(&record.run_id);
        }

        // A reaped run has no daemon event, so the server synthesizes the
        // contract terminal from its own verdict — carrying the real outcome,
        // which is what keeps `timed_out` and `host_stale` distinguishable
        // from a plain `failed`.
        let terminal = event.unwrap_or_else(|| {
            let body = ProviderEvent::TurnCompleted {
                provider_thread_id: None,
                status: outcome.turn_status(),
                error: (outcome != RunOutcome::Completed).then(|| TurnError {
                    message: error.clone().unwrap_or_default(),
                }),
                provider_checkpoint_id: None,
            };
            RunEvent::terminal(
                record.thread_id.clone(),
                record.project_id.clone(),
                record.run_id.clone(),
                now,
                outcome,
                body,
            )
        });
        if let Err(error) = self.publish_run_event(&record, terminal) {
            return FinishRunResult::PublishFailed(error.to_string());
        }

        // The terminal append is the first commit point. Keep the record until
        // the follow-up thread-status append also succeeds, so a transient
        // relay failure can retry the same lifecycle without another terminal.
        self.runs.mark_terminal(&record.run_id, outcome);
        let record = self
            .runs
            .get(&record.run_id)
            .expect("a run remains registered until its settlement completes");
        self.settle_finished_thread(&record, outcome, error.as_deref(), now, drain_queue)
    }

    /// Publishes the terminal thread-status event and then clears the entity-
    /// side run. The status mutation follows its relay append; this keeps a
    /// failed append from making a thread look idle/error before the durable log
    /// says so.
    ///
    /// `error` is the same text the terminal event carries, and it is what an
    /// automation run behind this provider run closes with: the automation's
    /// history should say what went wrong, not just that something did.
    fn settle_finished_thread(
        &self,
        record: &RunRecord,
        outcome: RunOutcome,
        error: Option<&str>,
        now: u64,
        drain_queue: bool,
    ) -> FinishRunResult {
        let trigger = match outcome {
            RunOutcome::Completed => ThreadTrigger::RunCompleted,
            RunOutcome::Cancelled => ThreadTrigger::RunCancelled,
            RunOutcome::Failed | RunOutcome::TimedOut | RunOutcome::HostStale => {
                ThreadTrigger::RunFailed
            }
        };
        let Some(record) = self.runs.get(&record.run_id) else {
            return FinishRunResult::AlreadyFinished;
        };
        let pending = record.pending_status_event.clone().or_else(|| {
            let thread = self.registry.thread(&record.thread_id)?;
            let to = thread.status.transition(trigger)?;
            Some(PendingStatusChange {
                thread_id: thread.id,
                project_id: thread.project_id,
                from: thread.status,
                to,
                at_ms: now,
            })
        });
        if let Some(pending) = pending {
            // Store the exact event before appending it. If the backend rejects
            // the append, the same event can be retried rather than rebuilt
            // with a different timestamp or source status.
            self.runs
                .set_pending_status_event(&record.run_id, pending.clone());
            let change = pending.event();
            if let Err(error) = self.publish_domain_event(&change) {
                return FinishRunResult::PublishFailed(error.to_string());
            }
            // Apply only after the relay has accepted the event. Replay sees
            // the same mutation if the process dies between these two steps.
            self.registry.apply_event(&change);
            self.runs.clear_pending_status_event(&record.run_id);
        }
        let _ = self.registry.clear_thread_run(&record.thread_id, now);
        self.runs.remove(&record.run_id);
        // An automation run is a *view* of a provider run: its terminal state
        // is what ends it, so the view is closed here, at the one place every
        // terminal path passes through (a report, a reaped run, a stop, a
        // restart). The automation registry has its own lock and this is called
        // after the run is gone, so the two tables cannot deadlock; the durable
        // write is the periodic snapshot, exactly as it is for the provider run
        // table itself.
        self.close_automation_run_for(&record, outcome, error, now);
        // A turn that ended cannot still be waiting on an answer, and a thread
        // that just became idle is exactly when the queue is worth draining.
        // Both are ordered after the status change so the thread a subscriber
        // sees is already out of `working` when the queued turn starts.
        self.cancel_thread_interactions(&record.thread_id, now);
        if drain_queue {
            self.drain_thread_queue(&record.thread_id);
        }
        FinishRunResult::Finished
    }

    /// Closes the automation run that became this provider run, if there is one.
    ///
    /// Most provider runs are ordinary turns and this finds nothing. When it
    /// does find one, the automation's own policy applies: a failed scheduled
    /// run retries sooner than its next window, three consecutive failures
    /// pause the automation, and a success clears the failure counter. The
    /// thread id travels with the outcome so the automation's history keeps the
    /// `thread ↔ run` mapping a client needs to open the conversation.
    fn close_automation_run_for(
        &self,
        record: &RunRecord,
        outcome: RunOutcome,
        error: Option<&str>,
        now: u64,
    ) {
        let thread_id = Some(record.thread_id.clone());
        let outcome = match outcome {
            RunOutcome::Completed => AutomationRunOutcome::Succeeded {
                thread_id,
                output: None,
                exit_code: None,
            },
            RunOutcome::Cancelled => AutomationRunOutcome::Cancelled {
                reason: error
                    .map(str::to_owned)
                    .unwrap_or_else(|| "the run was stopped before it finished".to_owned()),
            },
            RunOutcome::Failed | RunOutcome::TimedOut | RunOutcome::HostStale => {
                AutomationRunOutcome::Failed {
                    error: error
                        .map(str::to_owned)
                        .unwrap_or_else(|| outcome.as_str().to_owned()),
                    thread_id,
                    output: None,
                    exit_code: None,
                }
            }
        };
        if let Some(closed) =
            self.automations
                .close_run_by_provider_run(&record.run_id.to_string(), &outcome, now)
        {
            eprintln!(
                "loom-server: automation run {} ({}) closed as {}",
                closed.id,
                closed.automation_id,
                closed.state.as_str()
            );
            // An automation run that ended is a run a client is rendering, so
            // its project is told to refetch — the same frame the script path
            // publishes when it settles one.
            if let Some(project_id) = self.automations.project_of_run(&closed.id) {
                self.publish_automations_changed(&project_id.to_string());
            }
        }
    }

    /// Fails a thread before a provider dispatch exists.
    ///
    /// The preflight attempt uses the same record and terminal commit point as
    /// a provider run. If relay publication fails part-way through the
    /// lifecycle, the record and the thread's `working` state remain so a later
    /// reconciliation can append the missing events without starting another
    /// run or leaving a start without a terminal.
    fn fail_thread(
        &self,
        thread: &Thread,
        run_id: RunId,
        reason: String,
        now: u64,
    ) -> Result<(), String> {
        let record = RunRecord {
            run_id,
            thread_id: thread.id.clone(),
            project_id: thread.project_id.clone(),
            // There is no owning execution host for a preflight attempt. The
            // value is only needed to keep the record shape total; it is never
            // used to dispatch or to accept a provider report.
            host_id: HostId::mint(),
            cwd: String::new(),
            started_at_ms: now,
            deadline_ms: now,
            turn_started: false,
            provider_thread_id: None,
            provider_error_reported: false,
            failure_reason: Some(reason.clone()),
            terminal_published: false,
            terminal_outcome: None,
            pending_status_event: None,
        };
        self.runs.insert(record.clone());
        match self.finish_run_with_locked(&record, RunOutcome::Failed, Some(reason), None, now) {
            FinishRunResult::Finished | FinishRunResult::AlreadyFinished => Ok(()),
            FinishRunResult::PublishFailed(error) => {
                eprintln!("loom-server: failed to publish preflight run event: {error}");
                Err(error)
            }
        }
    }

    /// Publishes one run event to the thread scope.
    ///
    /// `now` is only used when the event's identity needs re-stamping; the
    /// daemon's event already carries its own timestamp, which is preserved so
    /// replay is byte-identical to what the daemon sent.
    fn publish_run_event(&self, record: &RunRecord, event: RunEvent) -> RelayResult<()> {
        // Rebuild the envelope so a malformed daemon scope or outer identity
        // cannot leak into the thread log. The provider body remains verbatim.
        let outcome = event.outcome;
        let mut event = RunEvent::new(
            record.thread_id.clone(),
            record.project_id.clone(),
            record.run_id.clone(),
            event.at_ms,
            event.event.body,
        );
        event.outcome = outcome;
        self.publish_domain_event(&DomainEvent::ThreadRunEvent {
            run: Box::new(event),
        })
        .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::AppConfig;
    use loom_domain::{EnvironmentKind, MessageRole, RunId, ThreadStatus};
    use loom_relay::backend::memory::MemoryBackend;
    use loom_relay::backend::{LogRecord, RelayBackend};
    use loom_relay::{EventId, RelayError, ShardId};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    struct FailOnAppendBackend {
        inner: MemoryBackend,
        append_count: AtomicUsize,
        fail_at: AtomicUsize,
    }

    impl FailOnAppendBackend {
        fn new(fail_at: usize) -> Self {
            Self {
                inner: MemoryBackend::new(2_000),
                append_count: AtomicUsize::new(0),
                fail_at: AtomicUsize::new(fail_at),
            }
        }

        fn disable_failure(&self) {
            self.fail_at.store(0, Ordering::Release);
        }

        fn fail_on_append_after_next(&self) {
            let after_next = self.append_count.load(Ordering::Acquire).saturating_add(2);
            self.fail_at.store(after_next, Ordering::Release);
        }
    }

    impl RelayBackend for FailOnAppendBackend {
        fn shard_count(&self) -> u8 {
            self.inner.shard_count()
        }

        fn append(&self, shard: ShardId, record: LogRecord) -> loom_relay::Result<()> {
            let append_number = self.append_count.fetch_add(1, Ordering::AcqRel) + 1;
            if self.fail_at.load(Ordering::Acquire) == append_number {
                return Err(RelayError::backend(format!(
                    "injected append failure at append {append_number}"
                )));
            }
            self.inner.append(shard, record)
        }

        fn read_after(
            &self,
            shard: ShardId,
            after: Option<EventId>,
            limit: usize,
        ) -> loom_relay::Result<Vec<LogRecord>> {
            self.inner.read_after(shard, after, limit)
        }

        fn trim(&self, shard: ShardId, before_ms: u64) -> loom_relay::Result<u64> {
            self.inner.trim(shard, before_ms)
        }

        fn len(&self, shard: ShardId) -> loom_relay::Result<usize> {
            self.inner.len(shard)
        }
    }

    /// A state with reconciliation disabled, so a test drives it explicitly.
    fn state() -> AppState {
        AppState::build(AppConfig {
            reconcile_interval: Duration::ZERO,
            ..AppConfig::default()
        })
        .unwrap()
    }

    fn state_with_backend(backend: Arc<FailOnAppendBackend>) -> AppState {
        AppState::build_for_test(
            AppConfig {
                reconcile_interval: Duration::ZERO,
                ..AppConfig::default()
            },
            backend,
        )
        .unwrap()
    }

    /// Enrolls a connected host directly, without a socket. The reconciler only
    /// reads the host registry, so a socket is not needed to exercise it.
    fn enroll_host(state: &AppState) -> HostId {
        let (host, _) = state
            .registry
            .enroll_host(None, "laptop".into(), loom_relay::now_ms())
            .unwrap();
        host.id
    }

    /// A thread bound to a `ready` unmanaged environment on a connected host.
    ///
    /// The path need not exist on the server: existence is the daemon's check
    /// at spawn time, which is what keeps a remote host's workspace valid.
    fn thread_with_workspace(state: &AppState, path: &str) -> (HostId, Thread, String) {
        let host_id = enroll_host(state);
        let (environment, _) = state
            .registry
            .create_environment(
                Some(state.registry.personal_project_id()),
                host_id.clone(),
                EnvironmentKind::Unmanaged,
                Some(path.into()),
                loom_relay::now_ms(),
            )
            .unwrap();
        let (thread, _) = state
            .registry
            .create_thread(
                Some(state.registry.personal_project_id()),
                Some("t".into()),
                Some(environment.id),
                1,
            )
            .unwrap();
        (host_id, thread, path.into())
    }

    fn thread_run_events(state: &AppState, thread_id: &ThreadId) -> Vec<serde_json::Value> {
        let frames = state
            .relay
            .replay_scope(&Scope::Thread(thread_id.to_string()), 100)
            .unwrap();
        let mut run_events = Vec::new();
        for frame in &frames {
            let Ok(value) = serde_json::from_slice::<serde_json::Value>(&frame.payload) else {
                continue;
            };
            let Some(payload) = value["payload"].as_str() else {
                continue;
            };
            let Ok(event) = serde_json::from_str::<serde_json::Value>(payload) else {
                continue;
            };
            if event["type"].as_str() == Some("thread_run_event") {
                run_events.push(event);
            }
        }
        run_events
    }

    fn count_run_events(state: &AppState, thread_id: &ThreadId) -> (usize, usize) {
        let frames = state
            .relay
            .replay_scope(&Scope::Thread(thread_id.to_string()), 100)
            .unwrap();
        let mut status_changes = 0;
        for frame in &frames {
            let Ok(value) = serde_json::from_slice::<serde_json::Value>(&frame.payload) else {
                continue;
            };
            let Some(payload) = value["payload"].as_str() else {
                continue;
            };
            let Ok(event) = serde_json::from_str::<serde_json::Value>(payload) else {
                continue;
            };
            if event["type"].as_str() == Some("thread_status_changed") {
                status_changes += 1;
            }
        }
        (thread_run_events(state, thread_id).len(), status_changes)
    }

    #[test]
    fn the_registry_tracks_and_reaps_runs() {
        let registry = RunRegistry::new();
        let record = RunRecord {
            run_id: RunId::mint(),
            thread_id: ThreadId::mint(),
            project_id: ProjectId::mint(),
            host_id: HostId::mint(),
            cwd: "/srv/project-a".into(),
            started_at_ms: 1,
            deadline_ms: 10,
            turn_started: false,
            provider_thread_id: None,
            provider_error_reported: false,
            failure_reason: None,
            terminal_published: false,
            terminal_outcome: None,
            pending_status_event: None,
        };
        registry.insert(record.clone());
        assert_eq!(registry.get(&record.run_id), Some(record.clone()));
        assert_eq!(registry.for_host(&record.host_id), vec![record.clone()]);
        assert!(registry.expired(9).is_empty());
        assert_eq!(registry.expired(10), vec![record.clone()]);
        assert_eq!(registry.remove(&record.run_id), Some(record));
        assert!(registry.is_empty());
    }

    #[tokio::test]
    async fn a_run_with_no_environment_is_failed_on_the_spot() {
        let state = state();
        let (thread, _) = state
            .registry
            .create_thread(
                Some(state.registry.personal_project_id()),
                Some("t".into()),
                None,
                1,
            )
            .unwrap();
        state
            .registry
            .post_message(&thread.id, MessageRole::User, "hi".into(), 2)
            .unwrap();
        let thread = state.registry.thread(&thread.id).unwrap();
        assert_eq!(thread.status, ThreadStatus::Working);

        // No environment means no workspace. The dispatch is refused rather
        // than letting a provider run in the daemon's own cwd.
        let outcome = state.dispatch_thread(&thread, "hi");
        assert!(matches!(outcome, DispatchOutcome::NoEnvironment { .. }));
        assert_eq!(
            state.registry.thread(&thread.id).unwrap().status,
            ThreadStatus::Error
        );
        assert_eq!(count_run_events(&state, &thread.id), (3, 1));
        let events = thread_run_events(&state, &thread.id);
        assert_eq!(
            events
                .iter()
                .map(|event| event["event"]["type"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["turn/started", "provider/error", "turn/completed"]
        );
        let run_id = match outcome {
            DispatchOutcome::NoEnvironment { run_id, .. } => run_id,
            _ => unreachable!(),
        };
        assert!(events.iter().all(|event| {
            event["run_id"] == run_id.to_string()
                && event["event"]["scope"]["turnId"] == run_id.to_string()
        }));
        assert_eq!(
            events[1]["event"]["message"],
            "thread has no environment bound; bind one before dispatching"
        );
        assert_eq!(
            events[2]["event"]["error"]["message"],
            "thread has no environment bound; bind one before dispatching"
        );
        state.shutdown();
    }

    #[tokio::test]
    async fn a_partial_preflight_publish_is_retried_without_duplicate_start() {
        let backend = Arc::new(FailOnAppendBackend::new(2));
        let state = state_with_backend(backend.clone());
        let (thread, _) = state
            .registry
            .create_thread(
                Some(state.registry.personal_project_id()),
                Some("partial preflight".into()),
                None,
                1,
            )
            .unwrap();
        state
            .registry
            .post_message(&thread.id, MessageRole::User, "hi".into(), 2)
            .unwrap();
        let thread = state.registry.thread(&thread.id).unwrap();

        let run_id = match state.dispatch_thread(&thread, "hi") {
            DispatchOutcome::PublishFailed { run_id, .. } => run_id,
            other => panic!("expected an injected publish failure, got {other:?}"),
        };
        assert_eq!(
            state.registry.thread(&thread.id).unwrap().status,
            ThreadStatus::Working
        );
        assert_eq!(state.runs.for_thread(&thread.id).unwrap().run_id, run_id);
        let partial = thread_run_events(&state, &thread.id);
        assert_eq!(partial.len(), 1);
        assert_eq!(partial[0]["event"]["type"], "turn/started");

        backend.disable_failure();
        let summary = state.reconcile_runs(loom_relay::now_ms().saturating_add(1));
        assert_eq!(summary.timed_out_runs, 0);
        assert!(state.runs.is_empty());
        assert_eq!(
            state.registry.thread(&thread.id).unwrap().status,
            ThreadStatus::Error
        );

        let events = thread_run_events(&state, &thread.id);
        assert_eq!(
            events
                .iter()
                .map(|event| event["event"]["type"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["turn/started", "provider/error", "turn/completed"]
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event["run_id"] == run_id.to_string())
                .count(),
            3
        );
        state.shutdown();
    }

    #[tokio::test]
    async fn a_status_publish_failure_keeps_the_terminal_run_retryable() {
        let backend = Arc::new(FailOnAppendBackend::new(0));
        let state = state_with_backend(backend.clone());
        let (host_id, thread, _) = thread_with_workspace(&state, "/srv/project-a");
        state
            .registry
            .post_message(&thread.id, MessageRole::User, "hi".into(), 2)
            .unwrap();
        let thread = state.registry.thread(&thread.id).unwrap();
        let run = match state.dispatch_thread(&thread, "hi") {
            DispatchOutcome::Dispatched(run) => run,
            other => panic!("expected a dispatch, got {other:?}"),
        };

        let start = RunEvent::started(
            thread.id.clone(),
            thread.project_id.clone(),
            run.run_id.clone(),
            3,
            "provider-1",
        );
        assert_eq!(
            state.apply_run_report(
                &host_id,
                ProviderReport {
                    host_id: host_id.clone(),
                    event: start,
                },
            ),
            ReportOutcome::Applied
        );

        backend.fail_on_append_after_next();
        let terminal = RunEvent::completed(
            thread.id.clone(),
            thread.project_id.clone(),
            run.run_id.clone(),
            4,
            Some("provider-1".into()),
        );
        assert!(matches!(
            state.apply_run_report(
                &host_id,
                ProviderReport {
                    host_id: host_id.clone(),
                    event: terminal,
                },
            ),
            ReportOutcome::PublishFailed { .. }
        ));
        assert_eq!(state.runs.len(), 1);
        assert_eq!(
            state.registry.thread(&thread.id).unwrap().status,
            ThreadStatus::Working
        );

        backend.disable_failure();
        assert_eq!(state.reconcile_runs(5), ReconcileSummary::default());
        assert!(state.runs.is_empty());
        assert_eq!(
            state.registry.thread(&thread.id).unwrap().status,
            ThreadStatus::Idle
        );
        let events = thread_run_events(&state, &thread.id);
        assert_eq!(
            events
                .iter()
                .map(|event| event["event"]["type"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["turn/started", "turn/completed"]
        );
        assert_eq!(count_run_events(&state, &thread.id).1, 1);
        state.shutdown();
    }

    #[tokio::test]
    async fn an_environment_that_is_not_ready_or_has_no_path_is_failed_with_the_reason() {
        let state = state();
        let host_id = enroll_host(&state);
        let (creating, _) = state
            .registry
            .create_environment(
                Some(state.registry.personal_project_id()),
                host_id.clone(),
                EnvironmentKind::Managed,
                None,
                1,
            )
            .unwrap();
        let (not_ready_thread, _) = state
            .registry
            .create_thread(
                Some(state.registry.personal_project_id()),
                Some("not-ready".into()),
                Some(creating.id),
                2,
            )
            .unwrap();
        state
            .registry
            .post_message(&not_ready_thread.id, MessageRole::User, "hi".into(), 3)
            .unwrap();
        let not_ready_thread = state.registry.thread(&not_ready_thread.id).unwrap();
        assert!(matches!(
            state.dispatch_thread(&not_ready_thread, "hi"),
            DispatchOutcome::NoEnvironment { .. }
        ));
        assert!(
            thread_run_events(&state, &not_ready_thread.id)[1]["event"]["message"]
                .as_str()
                .unwrap()
                .contains("creating and not ready")
        );

        let (missing_path, _) = state
            .registry
            .create_environment(
                Some(state.registry.personal_project_id()),
                host_id,
                EnvironmentKind::Managed,
                None,
                4,
            )
            .unwrap();
        state
            .registry
            .set_environment_status(&missing_path.id, EnvironmentStatus::Provisioning, 5)
            .unwrap();
        state
            .registry
            .set_environment_status(&missing_path.id, EnvironmentStatus::Ready, 6)
            .unwrap();
        let (missing_path_thread, _) = state
            .registry
            .create_thread(
                Some(state.registry.personal_project_id()),
                Some("missing-path".into()),
                Some(missing_path.id),
                7,
            )
            .unwrap();
        state
            .registry
            .post_message(&missing_path_thread.id, MessageRole::User, "hi".into(), 8)
            .unwrap();
        let missing_path_thread = state.registry.thread(&missing_path_thread.id).unwrap();
        assert!(matches!(
            state.dispatch_thread(&missing_path_thread, "hi"),
            DispatchOutcome::NoEnvironment { .. }
        ));
        assert!(
            thread_run_events(&state, &missing_path_thread.id)[1]["event"]["message"]
                .as_str()
                .unwrap()
                .contains("no workspace path")
        );
        state.shutdown();
    }

    #[tokio::test]
    async fn a_run_whose_workspace_host_is_detached_is_failed_on_the_spot() {
        let state = state();
        let (host_id, thread, _) = thread_with_workspace(&state, "/srv/project-a");
        state
            .registry
            .post_message(&thread.id, MessageRole::User, "hi".into(), 2)
            .unwrap();
        state
            .registry
            .mark_host_disconnected(&host_id, loom_relay::now_ms())
            .unwrap();
        let thread = state.registry.thread(&thread.id).unwrap();

        let outcome = state.dispatch_thread(&thread, "hi");
        assert!(matches!(outcome, DispatchOutcome::NoHost { .. }));
        assert_eq!(
            state.registry.thread(&thread.id).unwrap().status,
            ThreadStatus::Error
        );
        let events = thread_run_events(&state, &thread.id);
        assert_eq!(events.len(), 3);
        assert_eq!(
            events[1]["event"]["message"],
            format!("host {host_id} owns this thread's workspace but is not connected")
        );
        state.shutdown();
    }

    #[tokio::test]
    async fn an_out_of_order_report_gets_one_start_and_duplicate_terminal_is_ignored() {
        let state = state();
        let (host_id, thread, _) = thread_with_workspace(&state, "/srv/project-a");
        state
            .registry
            .post_message(&thread.id, MessageRole::User, "hi".into(), 2)
            .unwrap();
        let thread = state.registry.thread(&thread.id).unwrap();
        let run = match state.dispatch_thread(&thread, "hi") {
            DispatchOutcome::Dispatched(run) => run,
            other => panic!("expected a dispatch, got {other:?}"),
        };

        // The provider's first useful event may arrive before its start after
        // a reconnect. The server inserts the anchor before forwarding it.
        let output = RunEvent::new(
            thread.id.clone(),
            thread.project_id.clone(),
            run.run_id.clone(),
            3,
            ProviderEvent::ItemAgentMessageDelta {
                item_id: "assistant-1".into(),
                delta: "hello".into(),
                provider_thread_id: "provider-1".into(),
                parent_tool_call_id: None,
            },
        );
        let report = ProviderReport {
            host_id: host_id.clone(),
            event: output,
        };
        assert_eq!(
            state.apply_run_report(&host_id, report),
            ReportOutcome::Applied
        );
        assert_eq!(thread_run_events(&state, &thread.id).len(), 2);

        // A late duplicate start must not create a second projection anchor.
        assert_eq!(
            state.apply_run_report(
                &host_id,
                ProviderReport {
                    host_id: host_id.clone(),
                    event: RunEvent::started(
                        thread.id.clone(),
                        thread.project_id.clone(),
                        run.run_id.clone(),
                        4,
                        "provider-1",
                    ),
                },
            ),
            ReportOutcome::Applied
        );
        assert_eq!(thread_run_events(&state, &thread.id).len(), 2);

        let terminal = RunEvent::completed(
            thread.id.clone(),
            thread.project_id.clone(),
            run.run_id.clone(),
            5,
            Some("provider-1".into()),
        );
        assert_eq!(
            state.apply_run_report(
                &host_id,
                ProviderReport {
                    host_id: host_id.clone(),
                    event: terminal.clone(),
                },
            ),
            ReportOutcome::Applied
        );
        assert_eq!(state.runs.len(), 0);
        assert_eq!(
            state.apply_run_report(
                &host_id,
                ProviderReport {
                    host_id: host_id.clone(),
                    event: terminal,
                },
            ),
            ReportOutcome::Unknown
        );
        let events = thread_run_events(&state, &thread.id);
        assert_eq!(
            events
                .iter()
                .map(|event| event["event"]["type"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["turn/started", "item/agentMessage/delta", "turn/completed"]
        );
        state.shutdown();
    }

    #[tokio::test]
    async fn the_dispatch_carries_the_environment_workspace() {
        let state = state();
        let (host_id, thread, workspace) = thread_with_workspace(&state, "/srv/project-a");
        state.registry.set_provider_session_id(
            &thread.id,
            "acp-session-1",
            Some(loom_domain::ProviderSessionBinding::new("pi", &workspace).at(2)),
            2,
        );
        state
            .registry
            .post_message(&thread.id, MessageRole::User, "hi".into(), 3)
            .unwrap();
        let thread = state.registry.thread(&thread.id).unwrap();
        assert!(matches!(
            state.dispatch_thread(&thread, "hi"),
            DispatchOutcome::Dispatched(_)
        ));

        // The dispatched spec names the thread's environment path, which is the
        // whole point: the daemon no longer chooses a cwd of its own.
        let frames = state
            .relay
            .replay_scope(&Scope::Host(host_id.to_string()), 10)
            .unwrap();
        assert_eq!(frames.len(), 1);
        let frame: serde_json::Value = serde_json::from_slice(&frames[0].payload).unwrap();
        let dispatch: serde_json::Value =
            serde_json::from_str(frame["payload"].as_str().unwrap()).unwrap();
        assert_eq!(dispatch["provider"]["cwd"], workspace);
        assert_eq!(dispatch["provider_session_id"], "acp-session-1");
        state.shutdown();
    }

    #[tokio::test]
    async fn a_dispatched_run_is_reported_and_completes() {
        let state = state();
        let (host_id, thread, _) = thread_with_workspace(&state, "/srv/project-a");
        state
            .registry
            .post_message(&thread.id, MessageRole::User, "hi".into(), 2)
            .unwrap();
        let thread = state.registry.thread(&thread.id).unwrap();
        let run = match state.dispatch_thread(&thread, "hi") {
            DispatchOutcome::Dispatched(run) => run,
            other => panic!("expected a dispatch, got {other:?}"),
        };
        assert_eq!(state.runs.len(), 1);

        let report = ProviderReport {
            host_id: host_id.clone(),
            event: RunEvent::completed(
                thread.id.clone(),
                thread.project_id.clone(),
                run.run_id.clone(),
                3,
                None,
            ),
        };
        assert_eq!(
            state.apply_run_report(&host_id, report),
            ReportOutcome::Applied
        );
        assert_eq!(state.runs.len(), 0);
        assert_eq!(
            state.registry.thread(&thread.id).unwrap().status,
            ThreadStatus::Idle
        );
        state.shutdown();
    }

    #[tokio::test]
    async fn a_report_from_the_wrong_host_is_rejected() {
        let state = state();
        let (_, thread, _) = thread_with_workspace(&state, "/srv/project-a");
        // A host id nobody enrolled: the report must not be accepted for a run
        // owned by the environment's host.
        let other = HostId::mint();
        state
            .registry
            .post_message(&thread.id, MessageRole::User, "hi".into(), 2)
            .unwrap();
        let thread = state.registry.thread(&thread.id).unwrap();
        let run = match state.dispatch_thread(&thread, "hi") {
            DispatchOutcome::Dispatched(run) => run,
            other => panic!("expected a dispatch, got {other:?}"),
        };

        let report = ProviderReport {
            host_id: other.clone(),
            event: RunEvent::completed(
                thread.id.clone(),
                thread.project_id.clone(),
                run.run_id.clone(),
                3,
                None,
            ),
        };
        assert!(matches!(
            state.apply_run_report(&other, report),
            ReportOutcome::Mismatch(_)
        ));
        // The run stays in flight and the thread stays working.
        assert_eq!(state.runs.len(), 1);
        assert_eq!(
            state.registry.thread(&thread.id).unwrap().status,
            ThreadStatus::Working
        );
        state.shutdown();
    }

    #[tokio::test]
    async fn reconciliation_times_out_a_run_without_reports() {
        let state = AppState::build(AppConfig {
            reconcile_interval: Duration::ZERO,
            run_timeout: Duration::from_millis(1),
            ..AppConfig::default()
        })
        .unwrap();
        let (_, thread, _) = thread_with_workspace(&state, "/srv/project-a");
        state
            .registry
            .post_message(&thread.id, MessageRole::User, "hi".into(), 2)
            .unwrap();
        let thread = state.registry.thread(&thread.id).unwrap();
        assert!(matches!(
            state.dispatch_thread(&thread, "hi"),
            DispatchOutcome::Dispatched(_)
        ));

        tokio::time::sleep(Duration::from_millis(5)).await;
        let summary = state.reconcile_runs(now_ms());
        assert_eq!(summary.timed_out_runs, 1);
        assert_eq!(
            state.registry.thread(&thread.id).unwrap().status,
            ThreadStatus::Error
        );
        assert_eq!(state.runs.len(), 0);
        state.shutdown();
    }

    #[tokio::test]
    async fn reconciliation_detaches_a_host_that_stopped_heartbeating() {
        let state = AppState::build(AppConfig {
            reconcile_interval: Duration::ZERO,
            host_stale_after: Duration::from_millis(1),
            ..AppConfig::default()
        })
        .unwrap();
        let host_id = enroll_host(&state);

        tokio::time::sleep(Duration::from_millis(5)).await;
        let summary = state.reconcile_runs(now_ms());
        assert_eq!(summary.stale_hosts, 1);
        assert_eq!(
            state.registry.host(&host_id).unwrap().status,
            HostStatus::Disconnected
        );
        state.shutdown();
    }

    /// The session a run reports is bound to the agent and workspace that
    /// opened it, and a dispatch only carries it while both match.
    ///
    /// The failure this guards is quiet and expensive: dispatching a session id
    /// to an agent that never issued it, or resuming a conversation about a
    /// directory the run is not editing.
    #[tokio::test]
    async fn a_provider_session_is_bound_to_its_agent_and_workspace() {
        let state = state();
        let (host_id, thread, workspace) = thread_with_workspace(&state, "/srv/project-a");
        state
            .registry
            .post_message(&thread.id, MessageRole::User, "hi".into(), 2)
            .unwrap();
        let thread = state.registry.thread(&thread.id).unwrap();
        let run = match state.dispatch_thread(&thread, "hi") {
            DispatchOutcome::Dispatched(run) => run,
            other => panic!("expected a dispatch, got {other:?}"),
        };

        // The first turn reports its identity, and the binding is taken from
        // the run that opened the session.
        let identity = RunEvent::new(
            thread.id.clone(),
            thread.project_id.clone(),
            run.run_id.clone(),
            3,
            ProviderEvent::ThreadIdentity {
                provider_thread_id: "acp-session-1".into(),
            },
        );
        assert_eq!(
            state.apply_run_report(
                &host_id,
                ProviderReport {
                    host_id: host_id.clone(),
                    event: identity,
                }
            ),
            ReportOutcome::Applied
        );
        let stored = state.registry.thread(&thread.id).unwrap();
        let binding = stored
            .provider_session_binding
            .as_ref()
            .expect("the identity event records the binding");
        assert_eq!(binding.agent, state.provider_spec().name);
        assert_eq!(binding.cwd, workspace);
        assert_eq!(stored.provider_session_id.as_deref(), Some("acp-session-1"));

        // A completed run, then a second turn: the id travels with it.
        let terminal = RunEvent::completed(
            thread.id.clone(),
            thread.project_id.clone(),
            run.run_id.clone(),
            4,
            None,
        );
        state.apply_run_report(
            &host_id,
            ProviderReport {
                host_id: host_id.clone(),
                event: terminal,
            },
        );
        let follow_up = state
            .registry
            .post_message(&thread.id, MessageRole::User, "again".into(), 5)
            .unwrap();
        for event in &follow_up {
            state.publish_domain_event(event).unwrap();
        }
        let thread = state.registry.thread(&thread.id).unwrap();
        assert!(matches!(
            state.dispatch_thread(&thread, "again"),
            DispatchOutcome::Dispatched(_)
        ));
        let frames = state
            .relay
            .replay_scope(&Scope::Host(host_id.to_string()), 10)
            .unwrap();
        let last: serde_json::Value = serde_json::from_slice(&frames[1].payload).unwrap();
        let dispatch: serde_json::Value =
            serde_json::from_str(last["payload"].as_str().unwrap()).unwrap();
        assert_eq!(dispatch["provider_session_id"], "acp-session-1");

        // A binding to a different workspace — an environment re-bound between
        // runs, or a session established elsewhere — invalidates the session:
        // the id belongs to the old directory.
        state.registry.set_provider_session_id(
            &thread.id,
            "acp-session-1",
            Some(loom_domain::ProviderSessionBinding::new("pi", "/srv/project-b").at(6)),
            6,
        );
        let thread = state.registry.thread(&thread.id).unwrap();
        assert_eq!(
            thread.resumable_session_id(&state.provider_spec().name, "/srv/project-a"),
            None,
            "a session opened in one workspace must not be resumed in another"
        );

        // And so does a different agent, even in the same workspace.
        state.registry.set_provider_session_id(
            &thread.id,
            "acp-session-1",
            Some(loom_domain::ProviderSessionBinding::new("other-agent", workspace).at(7)),
            7,
        );
        let thread = state.registry.thread(&thread.id).unwrap();
        assert_eq!(
            thread.resumable_session_id(&state.provider_spec().name, "/srv/project-a"),
            None,
            "a session id is only meaningful to the agent that issued it"
        );

        state.shutdown();
    }

    #[tokio::test]
    async fn a_stale_host_reaps_its_in_flight_run() {
        let state = AppState::build(AppConfig {
            reconcile_interval: Duration::ZERO,
            host_stale_after: Duration::from_millis(1),
            ..AppConfig::default()
        })
        .unwrap();
        let (_, thread, _) = thread_with_workspace(&state, "/srv/project-a");
        state
            .registry
            .post_message(&thread.id, MessageRole::User, "hi".into(), 2)
            .unwrap();
        let thread = state.registry.thread(&thread.id).unwrap();
        assert!(matches!(
            state.dispatch_thread(&thread, "hi"),
            DispatchOutcome::Dispatched(_)
        ));

        tokio::time::sleep(Duration::from_millis(5)).await;
        let summary = state.reconcile_runs(now_ms());
        assert_eq!(summary.stale_runs, 1);
        assert_eq!(
            state.registry.thread(&thread.id).unwrap().status,
            ThreadStatus::Error
        );
        assert_eq!(state.runs.len(), 0);
        state.shutdown();
    }
}
