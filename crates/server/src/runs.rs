//! In-flight provider runs: dispatch, reporting, and reconciliation.
//!
//! This is the control plane's half of the provider contract in
//! [`loom_provider_protocol`]. Three responsibilities, and the reason each is
//! here rather than in a route handler:
//!
//! 1. **Dispatch goes through the relay.** [`AppState::dispatch_thread`] mints
//!    a run, records it, and publishes a [`RunDispatch`] to `host:{id}`. The
//!    handler never touches a daemon socket, so a daemon that is momentarily
//!    disconnected still gets the run on reconnect.
//! 2. **Reports become thread events.** [`AppState::apply_run_report`] turns a
//!    daemon observation into a `thread_run_event` and publishes it to the
//!    thread scope, in order. The daemon's socket is not the fan-out path.
//! 3. **Every run reaches a terminal state.** [`AppState::reconcile_runs`] is
//!    the backstop: a provider that never reports, a daemon that stops
//!    heartbeating, and a deadline that passed all end in exactly one
//!    [`RunEvent::Finished`], which moves the thread out of `working`.
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
    DomainEvent, HostId, HostStatus, ProjectId, RunEvent, RunId, RunOutcome, Thread, ThreadId,
    ThreadTrigger,
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
}

impl AppState {
    /// Mints a run, records it, and publishes the dispatch to `host:{id}`.
    ///
    /// The caller must already have moved `thread` into `working` (a user
    /// message does that). This method only decides *where* it runs and records
    /// that it is running.
    pub fn dispatch_thread(&self, thread: &Thread, prompt: &str) -> DispatchOutcome {
        let now = now_ms();
        let Some(host) = self.registry.primary_host(self.local_host_id()) else {
            return DispatchOutcome::NoHost {
                run_id: self.fail_unhosted(thread, now),
            };
        };

        let run_id = RunId::mint();
        let record = RunRecord {
            run_id: run_id.clone(),
            thread_id: thread.id.clone(),
            project_id: thread.project_id.clone(),
            host_id: host.id.clone(),
            started_at_ms: now,
            deadline_ms: now.saturating_add(self.run_timeout_ms()),
        };
        let dispatch = RunDispatch {
            run_id: run_id.clone(),
            thread_id: thread.id.clone(),
            project_id: thread.project_id.clone(),
            host_id: host.id.clone(),
            prompt: prompt.to_owned(),
            provider: self.provider_spec().clone(),
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

    /// Applies one daemon report.
    ///
    /// A report for an unknown run is dropped: that is what makes a daemon's
    /// post-reconnect redelivery idempotent. A report for a run this host does
    /// not own is rejected, so one machine cannot terminate another's turn.
    pub fn apply_run_report(&self, host_id: &HostId, report: ProviderReport) -> ReportOutcome {
        let now = now_ms();
        let Some(record) = self.runs.get(&report.run_id) else {
            return ReportOutcome::Unknown;
        };
        if &record.host_id != host_id {
            return ReportOutcome::Mismatch(format!(
                "run {} is owned by host {}, not {}",
                report.run_id, record.host_id, host_id
            ));
        }
        if record.thread_id != report.thread_id {
            return ReportOutcome::Mismatch(format!(
                "run {} belongs to thread {}, not {}",
                report.run_id, record.thread_id, report.thread_id
            ));
        }

        match report.event {
            RunEvent::Finished { outcome, error } => self.finish_run(&record, outcome, error, now),
            other => self.publish_run_event(&record, other, now),
        }
        ReportOutcome::Applied
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

        summary
    }

    /// Publishes the terminal event, clears the run, and moves the thread out
    /// of `working`.
    fn finish_run(&self, record: &RunRecord, outcome: RunOutcome, error: Option<String>, now: u64) {
        self.runs.remove(&record.run_id);
        self.publish_run_event(record, RunEvent::Finished { outcome, error }, now);

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
    }

    /// Fails a thread that never got a host to run on.
    ///
    /// Returns the synthetic run id reported in the terminal event. No record
    /// is inserted, because there is nothing to reconcile: the run is already
    /// terminal.
    fn fail_unhosted(&self, thread: &Thread, now: u64) -> RunId {
        let run_id = RunId::mint();
        self.publish_domain_event(&DomainEvent::ThreadRunEvent {
            thread_id: thread.id.clone(),
            project_id: thread.project_id.clone(),
            run_id: run_id.clone(),
            at_ms: now,
            event: RunEvent::Finished {
                outcome: RunOutcome::Failed,
                error: Some("no host is connected to execute the run".into()),
            },
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
    fn publish_run_event(&self, record: &RunRecord, event: RunEvent, now: u64) {
        let domain = DomainEvent::ThreadRunEvent {
            thread_id: record.thread_id.clone(),
            project_id: record.project_id.clone(),
            run_id: record.run_id.clone(),
            at_ms: now,
            event,
        };
        let _ = self.publish_domain_event(&domain);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::AppConfig;
    use loom_domain::{MessageRole, RunId, ThreadStatus};
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
    async fn a_run_with_no_host_fails_the_thread_on_the_spot() {
        let state = state();
        let (thread, _) = state
            .registry
            .create_thread(None, Some("t".into()), 1)
            .unwrap();
        state
            .registry
            .post_message(&thread.id, MessageRole::User, "hi".into(), 2)
            .unwrap();
        let thread = state.registry.thread(&thread.id).unwrap();
        assert_eq!(thread.status, ThreadStatus::Working);

        let outcome = state.dispatch_thread(&thread, "hi");
        assert!(matches!(outcome, DispatchOutcome::NoHost { .. }));
        assert_eq!(
            state.registry.thread(&thread.id).unwrap().status,
            ThreadStatus::Error
        );
        assert_eq!(count_run_events(&state, &thread.id), (1, 1));
        state.shutdown();
    }

    #[tokio::test]
    async fn a_dispatched_run_is_reported_and_completes() {
        let state = state();
        let host_id = enroll_host(&state);
        let (thread, _) = state
            .registry
            .create_thread(None, Some("t".into()), 1)
            .unwrap();
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
            run_id: run.run_id.clone(),
            thread_id: thread.id.clone(),
            event: RunEvent::Finished {
                outcome: RunOutcome::Completed,
                error: None,
            },
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
        // The dispatcher must have a connected host; only its identity in the
        // registry matters, so the id is not bound.
        enroll_host(&state);
        // A host id nobody enrolled: the report must not be accepted for a run
        // owned by `host_id`.
        let other = HostId::mint();
        let (thread, _) = state
            .registry
            .create_thread(None, Some("t".into()), 1)
            .unwrap();
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
            run_id: run.run_id.clone(),
            thread_id: thread.id.clone(),
            event: RunEvent::Finished {
                outcome: RunOutcome::Completed,
                error: None,
            },
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
        enroll_host(&state);
        let (thread, _) = state
            .registry
            .create_thread(None, Some("t".into()), 1)
            .unwrap();
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

    #[tokio::test]
    async fn a_stale_host_reaps_its_in_flight_run() {
        let state = AppState::build(AppConfig {
            reconcile_interval: Duration::ZERO,
            host_stale_after: Duration::from_millis(1),
            ..AppConfig::default()
        })
        .unwrap();
        enroll_host(&state);
        let (thread, _) = state
            .registry
            .create_thread(None, Some("t".into()), 1)
            .unwrap();
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
