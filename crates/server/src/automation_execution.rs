//! Executing an automation run: creating or reusing its thread, then
//! dispatching a turn through the existing run/ACP path.
//!
//! There is deliberately **no execution framework here**. A queued agent run
//! becomes a turn exactly the way a client's message does: the message is
//! appended through the existing path (which moves the thread to `working`) and
//! [`AppState::dispatch_thread`] resolves the environment, records the run and
//! publishes the dispatch to the owning host. The automation therefore inherits
//! the whole existing lifecycle — the run registry, the pre-dispatch failure
//! timeline, the provider reports, the thread status machine — and this module
//! only owns the *resolution* an automation needs and a client does not: which
//! environment a declared `environment` means, which thread a run happens in,
//! and what happens to the run when either cannot be resolved.
//!
//! # The environment an automation runs in
//!
//! The contract lets an execution name an environment four ways, and that
//! vocabulary is what a user sees (`ui/packages/automations/src/cli.ts`). This
//! server resolves them as follows:
//!
//! | declared | resolved to |
//! | --- | --- |
//! | `reuse { environmentId }` | that environment, unchanged (the dispatch preflight decides whether it is usable) |
//! | `host { workspace: unmanaged { path } }` | the project's unmanaged environment on that host with that path, created if it does not exist yet |
//! | anything else | **nothing** — the run fails, with a reason naming what to do |
//!
//! The last row is the honest half. A `managed-worktree` or `personal`
//! workspace has to be *provisioned* on the host, and the environment entity
//! carries no branch to provision from (`Environment::create` has no such
//! field), so "use the newest ready environment instead" would run the turn in
//! a workspace the caller did not ask for. A `project-default` has the same
//! problem: loom has no server-side default-workspace resolution, so choosing
//! one would be a guess. All three fail visibly, with a retryable reason that
//! the failure policy turns into the next attempt.
//!
//! # The thread a run happens in
//!
//! An execution that names a `targetThreadId` runs *in* that thread: it must
//! exist, belong to the automation's project, not be deleted or archived, and
//! be `idle` or `error` — a thread in any other state either has a run of its
//! own or is not writable, and an automation states what it wants rather than
//! joining a queue. Anything else fails the run with a reason.
//!
//! Without a target, each run gets its own thread in the automation's project.
//! That is what makes a run's history readable: one automation run, one
//! conversation, and the run row carries the thread id.

use loom_domain::automation::{
    AgentEnvironment, AgentExecution, Automation, AutomationRun, AutomationRunOutcome,
    WorkspaceKind,
};
use loom_domain::{EnvironmentId, EnvironmentKind, MessageRole, ThreadId, ThreadStatus};
use loom_relay::now_ms;

use crate::runs::{DispatchOutcome, RunRecord};
use crate::state::AppState;

/// How many queued runs one pass dispatches.
///
/// A pass is a sweep tick, so this bounds how long one tick can spend on
/// resolution and dispatch; the next tick takes the rest.
const DISPATCH_BATCH: usize = 16;

/// What one execution pass did, for the log line and for the tests.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AutomationExecutionReport {
    /// Queued runs the pass considered.
    pub considered: usize,
    /// Runs whose turn was dispatched to a host.
    pub dispatched: usize,
    /// Runs that failed before a dispatch, with the reason recorded.
    pub failed: usize,
    /// Script runs left queued: the script executor is the next stage.
    pub skipped_script: usize,
}

impl AutomationExecutionReport {
    /// Whether the pass changed anything that has to be persisted.
    pub fn changed(&self) -> bool {
        self.dispatched > 0 || self.failed > 0
    }

    /// One line for the operator log, or `None` when there is nothing to say.
    pub fn diagnostic(&self) -> Option<String> {
        if self.considered == 0 {
            return None;
        }
        Some(format!(
            "automation execution: {} queued run(s) considered, {} dispatched, {} failed before \
             dispatch, {} waiting on a script executor",
            self.considered, self.dispatched, self.failed, self.skipped_script
        ))
    }
}

impl AppState {
    /// Dispatches the queued agent runs of every automation, oldest first.
    ///
    /// This is the second half of the automation loop: the sweep decides *when*
    /// a run is owed, this decides *where* it runs. A run that cannot be
    /// dispatched is failed with its reason rather than left queued — a queued
    /// run holds its automation's single-flight slot, so a stuck queue entry
    /// would stop the automation from ever running again.
    pub fn execute_pending_automation_runs(&self, now: u64) -> AutomationExecutionReport {
        let mut report = AutomationExecutionReport::default();
        for run in self.automations.pending_runs(DISPATCH_BATCH) {
            report.considered += 1;
            let Some(automation) = self.automations.automation(&run.automation_id) else {
                report.failed += 1;
                self.fail_queued_run(
                    &run,
                    "the automation that owns this run is gone".to_owned(),
                    now,
                );
                continue;
            };
            let Some(execution) = automation.execution.agent().cloned() else {
                // A script run waits for the script executor. Nothing is wrong
                // with it, so it is not failed: it is not this stage's work.
                report.skipped_script += 1;
                continue;
            };
            match self.dispatch_automation_run(&automation, &execution, &run, now) {
                Ok(()) => report.dispatched += 1,
                Err(reason) => {
                    report.failed += 1;
                    self.fail_queued_run(&run, reason, now);
                }
            }
        }
        if let Some(diagnostic) = report.diagnostic() {
            eprintln!("loom-server: {diagnostic}");
        }
        report
    }

    /// One run: resolve its thread, then hand the turn to the existing path.
    ///
    /// The order is the point. The run moves to `running`, the thread is
    /// resolved (and created when the execution does not name one), and the
    /// turn goes through the same append-and-dispatch a client message uses. A
    /// pre-dispatch failure inside the dispatch is not an error here: it has
    /// already produced the legal started/error/terminal sequence on the
    /// thread, and this reports it so the automation run closes with the same
    /// reason.
    fn dispatch_automation_run(
        &self,
        automation: &Automation,
        execution: &AgentExecution,
        run: &AutomationRun,
        now: u64,
    ) -> Result<(), String> {
        self.warn_on_provider_mismatch(automation, execution);
        let thread_id = self.resolve_run_thread(automation, execution, now)?;
        self.automations
            .start_run(&run.id, now)
            .map_err(|error| error.to_string())?;

        let published = crate::http::append_thread_message_or_reason(
            self,
            &thread_id,
            MessageRole::User,
            execution.prompt.clone(),
        )?;
        let dispatched = match published.outcome {
            Some(DispatchOutcome::Dispatched(record)) => record,
            Some(DispatchOutcome::NoEnvironment { run_id, reason }) => {
                return Err(format!("no usable environment: {reason} (run {run_id})"))
            }
            Some(DispatchOutcome::NoHost { run_id, reason }) => {
                return Err(format!("no connected host: {reason} (run {run_id})"))
            }
            Some(DispatchOutcome::PublishFailed { run_id, error }) => {
                return Err(format!(
                    "the dispatch of run {run_id} could not be published: {error}"
                ))
            }
            Some(DispatchOutcome::AlreadyInFlight { run_id }) => {
                return Err(format!(
                    "thread {thread_id} is already running provider run {run_id}"
                ))
            }
            // The message was appended without starting a turn: the thread
            // moved out of the state that starts one between the resolution
            // above and the append. The prompt is in the thread either way, so
            // the run says so rather than pretending it dispatched.
            None => {
                return Err(format!(
                    "the prompt was added to thread {thread_id} but no turn started; the thread \
                     was already busy"
                ))
            }
        };
        self.automations
            .attach_run_dispatch(&run.id, &dispatched.thread_id, &run_id_of(&dispatched), now)
            .map_err(|error| {
                // The turn is in flight, but nothing can map it back to this
                // automation run — and a run that cannot be closed by its
                // provider report would hold its single-flight slot forever.
                // Failing it here keeps the automation moving, and the reason
                // names the real problem.
                format!("the dispatched turn could not be mapped to its automation run: {error}")
            })?;
        Ok(())
    }

    /// The thread this run happens in: the one it targets, or a new one.
    fn resolve_run_thread(
        &self,
        automation: &Automation,
        execution: &AgentExecution,
        now: u64,
    ) -> Result<ThreadId, String> {
        if let Some(target) = &execution.target_thread_id {
            let thread = self
                .registry
                .public_thread(target)
                .ok_or_else(|| format!("target thread {target} is not known"))?;
            if thread.project_id != automation.project_id {
                return Err(format!(
                    "target thread {target} belongs to project {}, not {}",
                    thread.project_id, automation.project_id
                ));
            }
            match thread.status {
                // An errored thread accepts a retry, which is what an
                // automation asking for a turn means.
                ThreadStatus::Idle | ThreadStatus::Error => Ok(thread.id),
                other => Err(format!(
                    "target thread {target} is {other} and cannot take a turn; the automation will \
                     try again"
                )),
            }
        } else {
            self.create_run_thread(automation, execution, now)
        }
    }

    /// Creates the thread a run without a target gets.
    ///
    /// The thread is created *with* the resolved environment rather than
    /// unbound: a thread that exists is then a thread that can run, and a
    /// resolution problem is reported as one instead of surfacing later as a
    /// dispatch failure.
    fn create_run_thread(
        &self,
        automation: &Automation,
        execution: &AgentExecution,
        now: u64,
    ) -> Result<ThreadId, String> {
        let environment_id = self.resolve_run_environment(automation, execution, now)?;
        let (thread, event) = self
            .registry
            .create_thread(
                Some(automation.project_id.clone()),
                Some(automation.name.clone()),
                Some(environment_id),
                now,
            )
            .map_err(|error| error.to_string())?;
        self.publish_domain_event(&event)
            .map_err(|error| format!("the thread for this run could not be published: {error}"))?;
        Ok(thread.id)
    }

    /// The environment an agent execution declares, resolved to one that exists.
    fn resolve_run_environment(
        &self,
        automation: &Automation,
        execution: &AgentExecution,
        now: u64,
    ) -> Result<EnvironmentId, String> {
        match &execution.environment {
            AgentEnvironment::Reuse { environment_id } => {
                let environment = self
                    .registry
                    .environment(environment_id)
                    .ok_or_else(|| format!("environment {environment_id} is not known"))?;
                if environment.project_id != automation.project_id {
                    return Err(format!(
                        "environment {environment_id} belongs to project {}, not {}",
                        environment.project_id, automation.project_id
                    ));
                }
                Ok(environment.id)
            }
            AgentEnvironment::Host {
                host_id,
                workspace: WorkspaceKind::Unmanaged { path, .. },
            } => {
                let Some(path) = path.clone() else {
                    return Err(
                        "an unmanaged workspace with no path cannot be resolved; give the \
                         automation a path or bind an environment with `reuse`"
                            .to_owned(),
                    );
                };
                let host_id = host_id.clone().ok_or_else(|| {
                    "an unmanaged workspace names a directory on a host, so it needs a hostId"
                        .to_owned()
                })?;
                // Find-or-create: an unmanaged environment *is* a directory on
                // a host and starts `ready`, so declaring one is enough to have
                // one. Reusing the existing row is what keeps a schedule from
                // growing a new environment per run.
                let existing = self
                    .registry
                    .environments_for_project(&automation.project_id)
                    .into_iter()
                    .find(|environment| {
                        environment.kind == EnvironmentKind::Unmanaged
                            && environment.host_id == host_id
                            && environment.path.as_deref() == Some(path.as_str())
                    });
                if let Some(environment) = existing {
                    return Ok(environment.id);
                }
                let (environment, events) = self
                    .registry
                    .create_environment(
                        Some(automation.project_id.clone()),
                        host_id,
                        EnvironmentKind::Unmanaged,
                        Some(path),
                        now,
                    )
                    .map_err(|error| error.to_string())?;
                for event in &events {
                    self.publish_domain_event(event).map_err(|error| {
                        format!("the environment for this run could not be published: {error}")
                    })?;
                }
                Ok(environment.id)
            }
            AgentEnvironment::Host {
                workspace: WorkspaceKind::ManagedWorktree { .. },
                ..
            } => Err(
                "a managed worktree has to be provisioned on its host, which an automation run \
                 does not do yet (the environment entity carries no branch to provision from); \
                 bind a ready environment with `reuse`, or name an explicit path"
                    .to_owned(),
            ),
            AgentEnvironment::Host {
                workspace: WorkspaceKind::Personal,
                ..
            } => Err(
                "a personal workspace has to be provisioned on its host, which an automation run \
                 does not do yet; bind a ready environment with `reuse`, or name an explicit path"
                    .to_owned(),
            ),
            AgentEnvironment::ProjectDefault => Err(format!(
                "project {} has no server-side default workspace yet; bind an environment with \
                 `reuse`, or name a host and an explicit path",
                automation.project_id
            )),
        }
    }

    /// Logs when the automation asks for a provider this server does not serve.
    ///
    /// Not a failure: a thread's recorded model behaves the same way. The
    /// dispatch carries the configured provider, and a user running several
    /// machines should be able to see why a turn landed where it did.
    fn warn_on_provider_mismatch(&self, automation: &Automation, execution: &AgentExecution) {
        let configured = self.provider_spec().name.clone();
        if execution.provider_id != configured {
            eprintln!(
                "loom-server: automation {} asks for provider {:?} but this server runs {:?}; the \
                 run is dispatched with the configured provider",
                automation.id, execution.provider_id, configured
            );
        }
    }

    /// Fails a queued run with a reason, applying the automation's policy.
    fn fail_queued_run(&self, run: &AutomationRun, reason: String, now: u64) {
        let outcome = AutomationRunOutcome::Failed {
            error: reason,
            thread_id: None,
            output: None,
            exit_code: None,
        };
        match self.automations.close_run(&run.id, &outcome, now) {
            Ok(closed) => eprintln!(
                "loom-server: automation run {} ({}) failed before dispatch: {}",
                closed.id,
                closed.automation_id,
                closed.error.unwrap_or_default()
            ),
            Err(error) => eprintln!(
                "loom-server: could not close automation run {}: {error}",
                run.id
            ),
        }
    }
}

/// The provider run a dispatch produced.
fn run_id_of(record: &RunRecord) -> String {
    record.run_id.to_string()
}

/// The clock the executor stamps its decisions with.
pub fn execution_now() -> u64 {
    now_ms()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runs::ReportOutcome;
    use crate::state::AppConfig;
    use loom_domain::automation::AutomationRunStatus;
    use loom_domain::automation::{
        AutomationOrigin, AutomationTrigger, NewAutomation, PermissionMode,
    };
    use loom_domain::{
        AutomationRunState, EnvironmentStatus, HostId, ProjectId, ReasoningLevel, RunEvent,
        ThreadStatus,
    };
    use loom_provider_protocol::ProviderReport;
    use loom_relay::{now_ms, Scope};
    use std::time::Duration;

    /// A server with no background loops: every test drives the pipeline.
    fn state() -> AppState {
        AppState::build(AppConfig {
            reconcile_interval: Duration::ZERO,
            schedule_interval: Duration::ZERO,
            snapshot_interval: Duration::ZERO,
            ..AppConfig::default()
        })
        .unwrap()
    }

    /// An enrolled, connected host.
    fn host(state: &AppState) -> HostId {
        let (host, events) = state
            .registry
            .enroll_host(None, "worker".into(), now_ms())
            .expect("enrolls");
        for event in &events {
            state.publish_domain_event(event).expect("publishes");
        }
        host.id
    }

    /// A ready environment on that host with a workspace.
    fn environment(
        state: &AppState,
        host_id: &HostId,
        project: &ProjectId,
        path: &str,
    ) -> EnvironmentId {
        let (environment, events) = state
            .registry
            .create_environment(
                Some(project.clone()),
                host_id.clone(),
                EnvironmentKind::Unmanaged,
                Some(path.to_owned()),
                now_ms(),
            )
            .expect("creates");
        for event in &events {
            state.publish_domain_event(event).expect("publishes");
        }
        assert_eq!(environment.status, EnvironmentStatus::Ready);
        environment.id
    }

    /// An agent automation in `project`, with the given environment.
    fn automation(
        state: &AppState,
        project: &ProjectId,
        environment: AgentEnvironment,
        target_thread_id: Option<ThreadId>,
    ) -> Automation {
        let new = NewAutomation {
            name: "nightly".into(),
            enabled: true,
            trigger: AutomationTrigger::Schedule {
                cron: "0 9 * * *".into(),
                timezone: "UTC".into(),
            },
            execution: loom_domain::automation::AutomationExecution::Agent(AgentExecution {
                prompt: "summarise the repository".into(),
                provider_id: "pi".into(),
                model: "pi/default".into(),
                reasoning_level: ReasoningLevel::Medium,
                service_tier: None,
                permission_mode: PermissionMode::Auto,
                environment,
                target_thread_id,
            }),
            origin: AutomationOrigin::Human,
            created_by_thread_id: None,
        };
        state
            .automations
            .create(project.clone(), new, now_ms())
            .expect("creates the automation")
    }

    /// The timeline of a thread scope, as event type tags.
    ///
    /// Read from the relay rather than from the entity view: the point of the
    /// pre-dispatch assertions is what a subscriber replays.
    fn thread_timeline(state: &AppState, thread_id: &ThreadId) -> Vec<String> {
        let frames = state
            .relay
            .replay_scope(&Scope::Thread(thread_id.to_string()), 100)
            .expect("replays");
        frames
            .iter()
            .filter_map(|frame| {
                let value: serde_json::Value = serde_json::from_slice(&frame.payload).ok()?;
                let payload = value["payload"].as_str()?;
                let event: serde_json::Value = serde_json::from_str(payload).ok()?;
                event["type"].as_str().map(str::to_owned)
            })
            .collect()
    }

    /// The run events of a thread, as their contract bodies.
    fn thread_run_events(state: &AppState, thread_id: &ThreadId) -> Vec<serde_json::Value> {
        let frames = state
            .relay
            .replay_scope(&Scope::Thread(thread_id.to_string()), 100)
            .expect("replays");
        frames
            .iter()
            .filter_map(|frame| {
                let value: serde_json::Value = serde_json::from_slice(&frame.payload).ok()?;
                let payload = value["payload"].as_str()?;
                let event: serde_json::Value = serde_json::from_str(payload).ok()?;
                (event["type"].as_str() == Some("thread_run_event")).then_some(event)
            })
            .collect()
    }

    #[tokio::test]
    async fn a_queued_agent_run_becomes_a_dispatched_turn() {
        let state = state();
        let host_id = host(&state);
        let project = state.registry.personal_project_id();
        let environment = environment(&state, &host_id, &project, "/srv/loom");
        let automation = automation(
            &state,
            &project,
            AgentEnvironment::Reuse {
                environment_id: environment.clone(),
            },
            None,
        );

        let (queued, _) = state
            .automations
            .queue_manual_run(
                &project.to_string(),
                &automation.id.to_string(),
                None,
                now_ms(),
            )
            .expect("queues");
        assert_eq!(queued.state, AutomationRunState::Pending);

        let report = state.execute_pending_automation_runs(now_ms());
        assert_eq!(report.dispatched, 1, "{report:?}");

        let run = state
            .automations
            .run(&queued.id)
            .expect("the run is stored");
        assert_eq!(run.state, AutomationRunState::Running);
        let thread_id = run.thread_id.clone().expect("the run knows its thread");
        let provider_run = run
            .provider_run_id
            .clone()
            .expect("the run knows its provider run");

        // The dispatch went through the ordinary path: a run record for the
        // thread, and the thread is `working`.
        let record = state.runs.for_thread(&thread_id).expect("a provider run");
        assert_eq!(record.run_id.to_string(), provider_run);
        assert_eq!(
            state
                .registry
                .thread(&thread_id)
                .expect("the thread")
                .status,
            ThreadStatus::Working
        );
        assert_eq!(
            state.registry.thread(&thread_id).unwrap().environment_id,
            Some(environment),
            "the thread is bound to the resolved environment"
        );

        // The prompt is in the thread as a user message and the thread moved to
        // `working`. The turn's own events come from the daemon: the server
        // publishes the dispatch to the host's scope and waits.
        assert_eq!(
            thread_timeline(&state, &thread_id),
            vec!["thread_message_added", "thread_status_changed"],
            "the message and the transition to working, and nothing the daemon has not reported"
        );

        // The mapping is durable in both directions.
        let mark = state
            .automations
            .thread_mark(&thread_id)
            .expect("the thread is marked as automation-produced");
        assert_eq!(mark.automation_id, automation.id);
        assert_eq!(mark.run_id, queued.id);
        state.shutdown();
    }

    #[tokio::test]
    async fn the_provider_run_ending_closes_the_automation_run_it_belongs_to() {
        let state = state();
        let host_id = host(&state);
        let project = state.registry.personal_project_id();
        let environment = environment(&state, &host_id, &project, "/srv/loom");
        let automation = automation(
            &state,
            &project,
            AgentEnvironment::Reuse {
                environment_id: environment,
            },
            None,
        );
        let (queued, _) = state
            .automations
            .queue_manual_run(
                &project.to_string(),
                &automation.id.to_string(),
                None,
                now_ms(),
            )
            .expect("queues");
        state.execute_pending_automation_runs(now_ms());
        let run = state.automations.run(&queued.id).expect("stored");
        let thread_id = run.thread_id.clone().expect("a thread");
        let provider_run = run.provider_run_id.clone().expect("a provider run");

        let report = ProviderReport {
            host_id: host_id.clone(),
            event: RunEvent::completed(
                thread_id.clone(),
                project.clone(),
                loom_domain::RunId::parse(&provider_run).expect("a run id"),
                now_ms(),
                None,
            ),
        };
        assert_eq!(
            state.apply_run_report(&host_id, report),
            ReportOutcome::Applied
        );

        let closed = state.automations.run(&queued.id).expect("still stored");
        assert_eq!(closed.state, AutomationRunState::Succeeded);
        assert_eq!(closed.thread_id, Some(thread_id.clone()));
        assert_eq!(closed.response().status, AutomationRunStatus::Succeeded);
        assert!(closed.finished_at.is_some());
        // The thread went back to idle, so the automation can run again.
        assert_eq!(
            state.registry.thread(&thread_id).unwrap().status,
            ThreadStatus::Idle
        );
        let automation = state
            .automations
            .automation(&automation.id)
            .expect("stored");
        assert_eq!(automation.consecutive_failures, 0);
        assert_eq!(
            automation.last_run_status,
            Some(AutomationRunState::Succeeded)
        );
        assert_eq!(automation.last_run_thread_id, Some(thread_id));
        state.shutdown();
    }

    #[tokio::test]
    async fn a_run_whose_environment_is_gone_fails_with_the_reason_and_retries() {
        let state = state();
        let project = state.registry.personal_project_id();
        let automation = automation(
            &state,
            &project,
            AgentEnvironment::Reuse {
                environment_id: EnvironmentId::mint(),
            },
            None,
        );
        let (queued, _) = state
            .automations
            .queue_manual_run(
                &project.to_string(),
                &automation.id.to_string(),
                None,
                now_ms(),
            )
            .expect("queues");

        let report = state.execute_pending_automation_runs(now_ms());
        assert_eq!(report.failed, 1, "{report:?}");

        let run = state.automations.run(&queued.id).expect("stored");
        assert_eq!(run.state, AutomationRunState::Failed);
        assert!(run
            .error
            .as_deref()
            .is_some_and(|error| error.contains("is not known")));
        assert!(run.thread_id.is_none(), "no thread was created");

        // The automation's own policy applies: a scheduled failure retries
        // rather than waiting for the next window.
        let automation = state
            .automations
            .automation(&automation.id)
            .expect("stored");
        assert_eq!(automation.consecutive_failures, 1);
        assert!(automation.next_run_at.is_some_and(|next| next > now_ms()));
        state.shutdown();
    }

    #[tokio::test]
    async fn a_dispatch_before_the_host_is_connected_leaves_a_legal_timeline_and_a_failed_run() {
        let state = state();
        let host_id = host(&state);
        let project = state.registry.personal_project_id();
        let environment = environment(&state, &host_id, &project, "/srv/loom");
        state
            .registry
            .mark_host_disconnected(&host_id, now_ms())
            .expect("disconnects");
        let automation = automation(
            &state,
            &project,
            AgentEnvironment::Reuse {
                environment_id: environment.clone(),
            },
            None,
        );
        let (queued, _) = state
            .automations
            .queue_manual_run(
                &project.to_string(),
                &automation.id.to_string(),
                None,
                now_ms(),
            )
            .expect("queues");

        let report = state.execute_pending_automation_runs(now_ms());
        assert_eq!(report.failed, 1, "{report:?}");

        // The thread the run created carries the W-584 pre-dispatch timeline.
        let run = state.automations.run(&queued.id).expect("stored");
        assert_eq!(run.state, AutomationRunState::Failed);
        assert!(run
            .error
            .as_deref()
            .is_some_and(|error| error.contains("not connected")));
        let thread_id = run.thread_id.clone().unwrap_or_else(|| {
            state
                .registry
                .threads()
                .first()
                .expect("the run created a thread")
                .id
                .clone()
        });
        assert_eq!(
            thread_timeline(&state, &thread_id),
            vec![
                "thread_message_added",
                "thread_status_changed",
                "thread_run_event",
                "thread_run_event",
                "thread_run_event",
                "thread_status_changed",
            ],
            "the message, working, then started/error/completed, then the failure"
        );
        let run_events = thread_run_events(&state, &thread_id);
        assert_eq!(run_events.len(), 3, "started, provider error, terminal");
        assert_eq!(run_events[0]["event"]["type"], "turn/started");
        assert_eq!(run_events[1]["event"]["type"], "provider/error");
        assert_eq!(run_events[2]["event"]["type"], "turn/completed");
        assert_eq!(
            state.registry.thread(&thread_id).unwrap().status,
            ThreadStatus::Error,
            "the thread is in error, not stuck in working"
        );
        state.shutdown();
    }

    #[tokio::test]
    async fn a_target_thread_is_reused_and_checked_before_the_turn() {
        let state = state();
        let host_id = host(&state);
        let project = state.registry.personal_project_id();
        let environment = environment(&state, &host_id, &project, "/srv/loom");
        let (target, target_events) = state
            .registry
            .create_thread(
                Some(project.clone()),
                Some("existing".into()),
                Some(environment.clone()),
                now_ms(),
            )
            .expect("creates");
        state
            .publish_domain_event(&target_events)
            .expect("publishes");
        let automation = automation(
            &state,
            &project,
            AgentEnvironment::Reuse {
                environment_id: environment,
            },
            Some(target.id.clone()),
        );
        let (queued, _) = state
            .automations
            .queue_manual_run(
                &project.to_string(),
                &automation.id.to_string(),
                None,
                now_ms(),
            )
            .expect("queues");
        assert_eq!(
            state.execute_pending_automation_runs(now_ms()).dispatched,
            1
        );

        let run = state.automations.run(&queued.id).expect("stored");
        assert_eq!(run.thread_id, Some(target.id.clone()));
        assert_eq!(
            state.registry.thread(&target.id).unwrap().status,
            ThreadStatus::Working,
            "the existing thread took the turn"
        );
    }

    #[tokio::test]
    async fn a_missing_or_busy_target_thread_fails_the_run_with_a_reason() {
        let state = state();
        let host_id = host(&state);
        let project = state.registry.personal_project_id();
        let environment = environment(&state, &host_id, &project, "/srv/loom");

        // A target that does not exist.
        let missing = automation(
            &state,
            &project,
            AgentEnvironment::Reuse {
                environment_id: environment.clone(),
            },
            Some(ThreadId::mint()),
        );
        let (queued, _) = state
            .automations
            .queue_manual_run(
                &project.to_string(),
                &missing.id.to_string(),
                None,
                now_ms(),
            )
            .expect("queues");
        assert_eq!(state.execute_pending_automation_runs(now_ms()).failed, 1);
        let run = state.automations.run(&queued.id).expect("stored");
        assert_eq!(run.state, AutomationRunState::Failed);
        assert!(run
            .error
            .as_deref()
            .is_some_and(|error| error.contains("target thread")));

        // A target that is already working.
        let (busy, busy_events) = state
            .registry
            .create_thread(
                Some(project.clone()),
                Some("busy".into()),
                Some(environment.clone()),
                now_ms(),
            )
            .expect("creates");
        state.publish_domain_event(&busy_events).expect("publishes");
        state
            .registry
            .transition_thread(&busy.id, loom_domain::ThreadTrigger::RunStarted, now_ms())
            .expect("moves to working");
        let blocked = automation(
            &state,
            &project,
            AgentEnvironment::Reuse {
                environment_id: environment,
            },
            Some(busy.id.clone()),
        );
        let (queued, _) = state
            .automations
            .queue_manual_run(
                &project.to_string(),
                &blocked.id.to_string(),
                None,
                now_ms(),
            )
            .expect("queues");
        assert_eq!(state.execute_pending_automation_runs(now_ms()).failed, 1);
        let run = state.automations.run(&queued.id).expect("stored");
        assert!(run
            .error
            .as_deref()
            .is_some_and(|error| error.contains("cannot take a turn")));
        state.shutdown();
    }

    #[tokio::test]
    async fn an_unmanaged_workspace_is_created_once_and_then_reused() {
        let state = state();
        let host_id = host(&state);
        let project = state.registry.personal_project_id();
        let automation = automation(
            &state,
            &project,
            AgentEnvironment::Host {
                host_id: Some(host_id.clone()),
                workspace: WorkspaceKind::Unmanaged {
                    path: Some("/srv/loom".into()),
                    branch: None,
                },
            },
            None,
        );
        let (first, _) = state
            .automations
            .queue_manual_run(
                &project.to_string(),
                &automation.id.to_string(),
                None,
                now_ms(),
            )
            .expect("queues");
        assert_eq!(
            state.execute_pending_automation_runs(now_ms()).dispatched,
            1
        );
        let first = state.automations.run(&first.id).expect("stored");
        let first_thread = first.thread_id.clone().expect("a thread");
        let environment = state
            .registry
            .thread(&first_thread)
            .expect("the thread")
            .environment_id
            .expect("bound");

        // The next window resolves to the same environment rather than a new
        // one per run.
        state.stop_thread(&first_thread);
        let (second, _) = state
            .automations
            .queue_manual_run(
                &project.to_string(),
                &automation.id.to_string(),
                Some("second".into()),
                now_ms(),
            )
            .expect("queues");
        assert_eq!(
            state.execute_pending_automation_runs(now_ms()).dispatched,
            1
        );
        let second = state.automations.run(&second.id).expect("stored");
        assert_eq!(
            state
                .registry
                .thread(&second.thread_id.expect("a thread"))
                .unwrap()
                .environment_id,
            Some(environment)
        );
        assert_eq!(
            state.registry.environments_for_project(&project).len(),
            1,
            "one environment, not one per run"
        );
        state.shutdown();
    }

    #[tokio::test]
    async fn an_environment_that_has_to_be_provisioned_fails_with_a_reason() {
        let state = state();
        let project = state.registry.personal_project_id();
        for (label, environment, expected) in [
            (
                "managed worktree",
                AgentEnvironment::Host {
                    host_id: Some(HostId::mint()),
                    workspace: WorkspaceKind::ManagedWorktree {
                        base_branch: loom_domain::automation::ManagedBaseBranch::Default,
                    },
                },
                "managed worktree",
            ),
            (
                "project default",
                AgentEnvironment::ProjectDefault,
                "no server-side default workspace",
            ),
        ] {
            let automation = automation(&state, &project, environment, None);
            let (queued, _) = state
                .automations
                .queue_manual_run(
                    &project.to_string(),
                    &automation.id.to_string(),
                    Some(label.into()),
                    now_ms(),
                )
                .expect("queues");
            assert_eq!(state.execute_pending_automation_runs(now_ms()).failed, 1);
            let run = state.automations.run(&queued.id).expect("stored");
            assert_eq!(run.state, AutomationRunState::Failed, "{label}");
            assert!(
                run.error
                    .as_deref()
                    .is_some_and(|error| error.contains(expected)),
                "{label}: {:?}",
                run.error
            );
        }
        state.shutdown();
    }

    #[tokio::test]
    async fn a_script_run_waits_for_the_next_stage_instead_of_failing() {
        let state = state();
        let project = state.registry.personal_project_id();
        use loom_domain::automation::{AutomationExecution, ScriptExecution, ScriptInterpreter};
        let new = NewAutomation {
            name: "backup".into(),
            enabled: true,
            trigger: AutomationTrigger::Schedule {
                cron: "0 3 * * *".into(),
                timezone: "UTC".into(),
            },
            execution: AutomationExecution::Script(ScriptExecution {
                script: Some("echo hi".into()),
                script_file: None,
                interpreter: Some(ScriptInterpreter::Bash),
                timeout_ms: loom_domain::automation::AUTOMATION_SCRIPT_TIMEOUT_DEFAULT_MS,
                env: None,
            }),
            origin: AutomationOrigin::Human,
            created_by_thread_id: None,
        };
        let automation = state
            .automations
            .create(project.clone(), new, now_ms())
            .expect("creates");
        let (queued, _) = state
            .automations
            .queue_manual_run(
                &project.to_string(),
                &automation.id.to_string(),
                None,
                now_ms(),
            )
            .expect("queues");

        let report = state.execute_pending_automation_runs(now_ms());
        assert_eq!(report.skipped_script, 1, "{report:?}");
        assert_eq!(report.failed, 0);
        assert_eq!(
            state.automations.run(&queued.id).expect("stored").state,
            AutomationRunState::Pending,
            "the run is still queued, not failed"
        );
        state.shutdown();
    }

    #[tokio::test]
    async fn a_dispatched_thread_mark_points_back_at_the_run() {
        let state = state();
        let host_id = host(&state);
        let project = state.registry.personal_project_id();
        let environment = environment(&state, &host_id, &project, "/srv/loom");
        let automation = automation(
            &state,
            &project,
            AgentEnvironment::Reuse {
                environment_id: environment,
            },
            None,
        );
        let (queued, _) = state
            .automations
            .queue_manual_run(
                &project.to_string(),
                &automation.id.to_string(),
                None,
                now_ms(),
            )
            .expect("queues");
        state.execute_pending_automation_runs(now_ms());
        let run = state.automations.run(&queued.id).expect("stored");
        let thread_id = run.thread_id.clone().expect("a thread");

        let mark = state.automations.thread_mark(&thread_id).expect("a mark");
        assert_eq!(mark.run_id, queued.id);
        assert_eq!(mark.automation_id, automation.id);
        // A thread a run produced is the one thing an automation may not be
        // created from: the recursion guard reads the mapping this wrote.
        let nested = NewAutomation {
            name: "nested".into(),
            enabled: true,
            trigger: AutomationTrigger::Schedule {
                cron: "0 9 * * *".into(),
                timezone: "UTC".into(),
            },
            execution: loom_domain::automation::AutomationExecution::Agent(AgentExecution {
                prompt: "again".into(),
                provider_id: "pi".into(),
                model: "pi/default".into(),
                reasoning_level: ReasoningLevel::Medium,
                service_tier: None,
                permission_mode: PermissionMode::Auto,
                environment: AgentEnvironment::Reuse {
                    environment_id: EnvironmentId::mint(),
                },
                target_thread_id: None,
            }),
            origin: AutomationOrigin::Agent,
            created_by_thread_id: Some(thread_id),
        };
        assert!(state.automations.create(project, nested, now_ms()).is_err());
        state.shutdown();
    }
}
