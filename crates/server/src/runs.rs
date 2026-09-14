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

use loom_domain::{
    DomainEvent, Environment, EnvironmentStatus, HostId, HostStatus, ProjectId, ProviderEvent,
    RunEvent, RunId, RunOutcome, Thread, ThreadId, ThreadTrigger, TurnError, TurnStatus,
};
use loom_provider_protocol::{ProviderReport, RunDispatch};
use loom_relay::{now_ms, Scope};
use serde::{Deserialize, Serialize};

use crate::state::AppState;

/// A run the control plane has dispatched and has not seen terminate.
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
}

/// Runs currently in flight, keyed by run id.
///
/// In-memory, like the rest of the domain registry today: a server restart
/// loses the table, and the runs it described are reaped by the next server's
/// reconciliation because nothing reports for them. Persisting it is a
/// separate concern with its own issue.
#[derive(Debug, Default)]
pub struct RunRegistry {
    inner: Mutex<HashMap<RunId, RunRecord>>,
}

impl RunRegistry {
    /// An empty table.
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a run. Returns the previous record if the id was reused, which
    /// cannot happen for minted ids but keeps the API total.
    pub fn insert(&self, record: RunRecord) -> Option<RunRecord> {
        self.lock().insert(record.run_id.clone(), record)
    }

    /// A run by id.
    pub fn get(&self, run_id: &RunId) -> Option<RunRecord> {
        self.lock().get(run_id).cloned()
    }

    /// Removes a run, returning it if it was in flight.
    pub fn remove(&self, run_id: &RunId) -> Option<RunRecord> {
        self.lock().remove(run_id)
    }

    /// Every in-flight run on one host.
    pub fn for_host(&self, host_id: &HostId) -> Vec<RunRecord> {
        let mut runs: Vec<RunRecord> = self
            .lock()
            .values()
            .filter(|run| &run.host_id == host_id)
            .cloned()
            .collect();
        runs.sort_by(|left, right| left.run_id.cmp(&right.run_id));
        runs
    }

    /// Every in-flight run.
    pub fn all(&self) -> Vec<RunRecord> {
        let mut runs: Vec<RunRecord> = self.lock().values().cloned().collect();
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
            .values()
            .filter(|run| run.deadline_ms <= now_ms)
            .cloned()
            .collect();
        runs.sort_by(|left, right| left.run_id.cmp(&right.run_id));
        runs
    }

    /// How many runs are in flight.
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// Whether no run is in flight.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<RunId, RunRecord>> {
        self.inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }
}

/// What happened when a dispatch was attempted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DispatchOutcome {
    /// The dispatch was appended to the host's scope and the run is in flight.
    Dispatched(RunRecord),
    /// The thread has no usable environment, so there is no workspace to run
    /// the provider in. The run was failed on the spot rather than letting a
    /// provider start in the daemon's own cwd.
    NoEnvironment {
        /// The synthetic run id reported in the terminal event.
        run_id: RunId,
    },
    /// No execution machine is connected. The run was failed on the spot so the
    /// thread does not sit in `working` waiting for a machine that is not
    /// there.
    NoHost {
        /// The synthetic run id reported in the terminal event.
        run_id: RunId,
    },
    /// The relay rejected the append; the run was failed on the spot.
    PublishFailed {
        /// The run that never started.
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopOutcome {
    /// A run was in flight and has been terminated.
    Stopped,
    /// The thread had no run in flight; there was nothing to terminate.
    NoRun,
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
        let now = now_ms();

        let environment = match self.resolve_environment(thread) {
            Ok(environment) => environment,
            Err(error) => {
                return DispatchOutcome::NoEnvironment {
                    run_id: self.fail_thread(thread, error, now),
                }
            }
        };

        // The workspace lives on exactly one machine, so the run must go to the
        // environment's host rather than to whichever host is "primary".
        let Some(host) = self.registry.host(&environment.host_id) else {
            return DispatchOutcome::NoHost {
                run_id: self.fail_thread(
                    thread,
                    format!("host {} is not known", environment.host_id),
                    now,
                ),
            };
        };
        if host.status != HostStatus::Connected {
            return DispatchOutcome::NoHost {
                run_id: self.fail_thread(
                    thread,
                    format!(
                        "host {} owns this thread's workspace but is not connected",
                        host.id
                    ),
                    now,
                ),
            };
        }

        // A `ready` environment always has a path for a managed one and, by
        // construction, for an unmanaged one. Treat `None` as an internal
        // inconsistency rather than dispatch a provider without a workspace.
        let Some(workspace) = environment.path.clone() else {
            return DispatchOutcome::NoEnvironment {
                run_id: self.fail_thread(
                    thread,
                    format!("environment {} has no workspace path", environment.id),
                    now,
                ),
            };
        };

        let run_id = RunId::mint();
        let record = RunRecord {
            run_id: run_id.clone(),
            thread_id: thread.id.clone(),
            project_id: thread.project_id.clone(),
            host_id: host.id.clone(),
            cwd: workspace.clone(),
            started_at_ms: now,
            deadline_ms: now.saturating_add(self.run_timeout_ms()),
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
                self.finish_run(&record, RunOutcome::Failed, Some(error.to_string()), now);
                DispatchOutcome::PublishFailed {
                    run_id,
                    error: error.to_string(),
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
        let Some(record) = self.runs.for_thread(thread_id) else {
            return StopOutcome::NoRun;
        };
        self.finish_run(
            &record,
            RunOutcome::Cancelled,
            Some("stopped by a client".to_owned()),
            now,
        );
        StopOutcome::Stopped
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
            let outcome = event.outcome.unwrap_or_else(|| {
                match event.terminal_status().unwrap_or(TurnStatus::Failed) {
                    TurnStatus::Completed => RunOutcome::Completed,
                    TurnStatus::Interrupted => RunOutcome::Cancelled,
                    TurnStatus::Failed => RunOutcome::Failed,
                }
            });
            let error = event.terminal_error().map(str::to_owned);
            self.finish_run_with(&record, outcome, error, Some(event), now);
        } else {
            self.learn_provider_session(&event, now);
            self.publish_run_event(&record, event, now);
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
                self.finish_run(
                    &record,
                    RunOutcome::HostStale,
                    Some(format!("host {host_id} stopped heartbeating")),
                    now,
                );
                summary.stale_runs += 1;
            }
        }

        // 3. The server-side deadline is the backstop for a daemon that is
        //    connected but wedged. A run whose provider never settles is failed
        //    here even though nothing reported it.
        for record in self.runs.expired(now) {
            if self.runs.get(&record.run_id).is_none() {
                continue;
            }
            self.finish_run(
                &record,
                RunOutcome::TimedOut,
                Some("run exceeded its deadline".into()),
                now,
            );
            summary.timed_out_runs += 1;
        }

        // 4. Queued messages whose time has come. A scheduled message needs no
        //    other event to become due, so the sweep is what delivers it; a
        //    message left queued by a crash between a run's terminal event and
        //    its drain is picked up by the same pass.
        summary.sent_queued_messages = self.drain_due_queued_messages();

        summary
    }

    /// Publishes the server's own terminal event, clears the run, and moves
    /// the thread out of `working`.
    fn finish_run(&self, record: &RunRecord, outcome: RunOutcome, error: Option<String>, now: u64) {
        self.finish_run_with(record, outcome, error, None, now);
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
    ) {
        self.runs.remove(&record.run_id);
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
        self.publish_run_event(record, terminal, now);

        let trigger = match outcome {
            RunOutcome::Completed => ThreadTrigger::RunCompleted,
            RunOutcome::Cancelled => ThreadTrigger::RunCancelled,
            RunOutcome::Failed | RunOutcome::TimedOut | RunOutcome::HostStale => {
                ThreadTrigger::RunFailed
            }
        };
        let _ = self.registry.clear_thread_run(&record.thread_id, now);
        // A thread already reconciled is not an error: the transition simply
        // does not apply and no second status event is produced.
        if let Ok(Some(change)) = self
            .registry
            .transition_thread(&record.thread_id, trigger, now)
        {
            let _ = self.publish_domain_event(&change);
        }
        // A turn that ended cannot still be waiting on an answer, and a thread
        // that just became idle is exactly when the queue is worth draining.
        // Both are ordered after the status change so the thread a subscriber
        // sees is already out of `working` when the queued turn starts.
        self.cancel_thread_interactions(&record.thread_id, now);
        self.drain_thread_queue(&record.thread_id);
    }

    /// Fails a thread that never got a run started.
    ///
    /// Returns the synthetic run id reported in the terminal event. No record
    /// is inserted, because there is nothing to reconcile: the run is already
    /// terminal.
    fn fail_thread(&self, thread: &Thread, reason: String, now: u64) -> RunId {
        let run_id = RunId::mint();
        self.publish_domain_event(&DomainEvent::ThreadRunEvent {
            run: Box::new(RunEvent::failed(
                thread.id.clone(),
                thread.project_id.clone(),
                run_id.clone(),
                now,
                RunOutcome::Failed.turn_status(),
                reason,
            )),
        })
        .ok();
        let _ = self.registry.clear_thread_run(&thread.id, now);
        if let Ok(Some(change)) =
            self.registry
                .transition_thread(&thread.id, ThreadTrigger::RunFailed, now)
        {
            let _ = self.publish_domain_event(&change);
        }
        run_id
    }

    /// Publishes one run event to the thread scope.
    ///
    /// `now` is only used when the event's identity needs re-stamping; the
    /// daemon's event already carries its own timestamp, which is preserved so
    /// replay is byte-identical to what the daemon sent.
    fn publish_run_event(&self, record: &RunRecord, event: RunEvent, now: u64) {
        let event = if event.thread_id == record.thread_id {
            event
        } else {
            RunEvent::new(
                record.thread_id.clone(),
                record.project_id.clone(),
                record.run_id.clone(),
                now,
                event.event.body,
            )
        };
        let _ = self.publish_domain_event(&DomainEvent::ThreadRunEvent {
            run: Box::new(event),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::AppConfig;
    use loom_domain::{EnvironmentKind, MessageRole, RunId, ThreadStatus};
    use std::time::Duration;

    /// A state with reconciliation disabled, so a test drives it explicitly.
    fn state() -> AppState {
        AppState::build(AppConfig {
            reconcile_interval: Duration::ZERO,
            ..AppConfig::default()
        })
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

    fn count_run_events(state: &AppState, thread_id: &ThreadId) -> (usize, usize) {
        let frames = state
            .relay
            .replay_scope(&Scope::Thread(thread_id.to_string()), 100)
            .unwrap();
        let mut run_events = 0;
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
            match event["type"].as_str() {
                Some("thread_run_event") => run_events += 1,
                Some("thread_status_changed") => status_changes += 1,
                _ => {}
            }
        }
        (run_events, status_changes)
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
        assert_eq!(count_run_events(&state, &thread.id), (1, 1));
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
