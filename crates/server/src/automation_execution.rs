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
    AgentEnvironment, AgentExecution, Automation, AutomationExecution, AutomationRun,
    AutomationRunOutcome, AutomationRunState, WorkspaceKind,
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

/// What a script run's report did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScriptReportOutcome {
    /// The run was settled from the report.
    Applied,
    /// The run is not in flight: already settled by a cancel or a reap, or
    /// never dispatched. Normal under redelivery, and under a cancel race.
    Stale,
    /// The report named a run this connection does not own, or one this host
    /// was not sent.
    Mismatch(String),
    /// No automation run has that id.
    Unknown,
}

/// Whether a script's output says nothing.
///
/// The reference implementation's rule: a script that printed only whitespace
/// is `skipped`, not `succeeded`, because the run's meaning was its output.
fn output_is_empty(output: &Option<String>) -> bool {
    output.as_deref().map(str::trim).is_none_or(str::is_empty)
}

/// Whether a script's last non-empty output line is a `{"wakeAgent": false}`
/// object.
///
/// Also the reference implementation's rule, and kept for the same reason: the
/// run's recorded status is part of the wire. bb's scripts could ask not to
/// wake the agent that reads their output; loom's script runs wake nothing, but
/// such a script is still recorded as skipped rather than succeeded, so a
/// migrated automation's history reads the way it did.
fn suppresses_wake_agent(output: &str) -> bool {
    let Some(last) = output
        .lines()
        .map(str::trim)
        .rev()
        .find(|line| !line.is_empty())
    else {
        return false;
    };
    serde_json::from_str::<serde_json::Value>(last)
        .ok()
        .is_some_and(|parsed| {
            matches!(
                parsed.get("wakeAgent"),
                Some(serde_json::Value::Bool(false))
            )
        })
}

/// What one execution pass did, for the log line and for the tests.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AutomationExecutionReport {
    /// Queued runs the pass considered.
    pub considered: usize,
    /// Runs whose turn was dispatched to a host.
    pub dispatched: usize,
    /// Runs that failed before a dispatch, with the reason recorded.
    pub failed: usize,
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
             dispatch",
            self.considered, self.dispatched, self.failed
        ))
    }
}

impl AppState {
    /// Tells every client watching this project to refetch its automations.
    ///
    /// bb's public protocol has no automation entity — automations were a
    /// plugin there and their invalidation rode the plugin's own realtime
    /// channel — while the pinned client's subscription targets and the
    /// `changed` frame are fixed. So an automation or run change is published
    /// as a **project** change: the frame says "this project changed, refetch
    /// it", which is what every other invalidation in this protocol means. A
    /// client subscribed to the project (or to the project list) receives it,
    /// and no new frame type, entity or target is invented.
    ///
    /// The granularity is deliberately one frame for both: a client cannot act
    /// on "an automation changed" differently from "a run changed" — both mean
    /// refetch the automations it holds for this project.
    pub(crate) fn publish_automations_changed(&self, project_id: &str) {
        let message = crate::protocol::ServerMessage::Changed {
            entity: crate::protocol::PublicEntity::Project,
            id: Some(project_id.to_owned()),
            metadata: None,
            changes: vec![crate::protocol::PublicChangeKind::ProjectUpdated],
        };
        if let Err(error) = self.publish_public_change(&message) {
            eprintln!(
                "loom-server: the invalidation for project {project_id}'s automations could not be \
                 published: {error}"
            );
        }
    }

    /// Dispatches the queued runs of every automation, oldest first.
    ///
    /// This is the second half of the automation loop: the sweep decides *when*
    /// a run is owed, this decides *where* it runs. An agent run becomes a turn
    /// through the thread path; a script run becomes a request to the machine
    /// that owns the workspace. A run that cannot be dispatched is failed with
    /// its reason rather than left queued — a queued run holds its automation's
    /// single-flight slot, so a stuck queue entry would stop the automation
    /// from ever running again.
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
            let dispatched = match &automation.execution {
                AutomationExecution::Agent(execution) => {
                    self.dispatch_automation_run(&automation, execution, &run, now)
                }
                AutomationExecution::Script(execution) => {
                    self.dispatch_script_run(&automation, execution, &run, now)
                }
            };
            match dispatched {
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

    /// One script run: resolve where it runs, then hand it to that machine.
    ///
    /// Nothing here executes anything. The request travels through the relay to
    /// the host's scope — so a worker that was reconnecting still receives it
    /// on replay — and the host's report is what ends the run.
    ///
    /// A script has no environment in the contract, so what "the owning worker"
    /// means is not a project's workspace but the machine the server already
    /// prefers for work it cannot place (the primary host, which on a
    /// single-machine deployment is the enrolled local machine). The workspace
    /// is that machine's own automation-script directory: the reference
    /// implementation ran scripts from its plugin directory for the same
    /// reason, and the host is the only side that can name a path in it.
    fn dispatch_script_run(
        &self,
        automation: &Automation,
        execution: &loom_domain::automation::ScriptExecution,
        run: &AutomationRun,
        now: u64,
    ) -> Result<(), String> {
        let host = self
            .registry
            .primary_host(self.local_host_id())
            .ok_or_else(|| {
                "no connected machine can run this automation's script; start a worker on the \
                 machine that owns it"
                    .to_owned()
            })?;
        if host.status != loom_domain::HostStatus::Connected {
            return Err(format!(
                "host {} is the machine that owns this automation's script but is not connected",
                host.id
            ));
        }
        let Some(data_dir) = host.data_dir.clone() else {
            return Err(format!(
                "host {} has not reported a data directory, so its script directory cannot be \
                 named",
                host.id
            ));
        };
        let cwd =
            loom_provider_protocol::automation_script_root(&data_dir, &automation.id.to_string());
        let dispatch = loom_provider_protocol::ScriptRunDispatch {
            run_id: run.id.clone(),
            automation_id: automation.id.clone(),
            project_id: automation.project_id.clone(),
            host_id: host.id.clone(),
            cwd,
            script: execution.script.clone(),
            script_file: execution.script_file.clone(),
            interpreter: execution.interpreter,
            env: execution.env.clone().unwrap_or_default(),
            timeout_ms: execution.timeout_ms,
            deadline_ms: now
                .saturating_add(execution.timeout_ms)
                .saturating_add(loom_domain::automation::AUTOMATION_SCRIPT_TIMEOUT_DEFAULT_MS),
            created_at_ms: now,
        };
        let payload =
            serde_json::to_vec(&dispatch).expect("a ScriptRunDispatch always serializes to JSON");
        if let Err(error) = self.publish(loom_relay::Scope::Host(host.id.to_string()), payload) {
            return Err(format!(
                "the script run could not be published to host {}: {error}",
                host.id
            ));
        }
        // The host is recorded before the run is `running`: a cancel and the
        // reaper both need it, and a running script with no host is a run
        // nothing can reach.
        self.automations
            .attach_script_dispatch(&run.id, &host.id, now)
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    /// Applies a script run's report.
    ///
    /// The run's own state decides what happens: a report for a run the server
    /// already settled (a cancel, a reap, a duplicated frame) is `Stale`, which
    /// is what makes a cancel race harmless. A host that is not the one the
    /// dispatch went to is refused, so one machine cannot end another's run.
    pub fn apply_script_run_report(
        &self,
        host_id: &loom_domain::HostId,
        report: loom_provider_protocol::ScriptRunReport,
    ) -> ScriptReportOutcome {
        let now = now_ms();
        let Some(run) = self.automations.run(&report.run_id) else {
            return ScriptReportOutcome::Unknown;
        };
        if &report.host_id != host_id {
            return ScriptReportOutcome::Mismatch(
                "report names a different host than this connection enrolled as".to_owned(),
            );
        }
        if run.host_id.as_ref() != Some(host_id) {
            return ScriptReportOutcome::Mismatch(format!(
                "run {} was not dispatched to host {host_id}",
                report.run_id
            ));
        }
        if run.state != AutomationRunState::Running {
            return ScriptReportOutcome::Stale;
        }
        let outcome = match report.outcome {
            loom_provider_protocol::ScriptRunOutcome::Refused { error } => {
                AutomationRunOutcome::Failed {
                    error,
                    thread_id: None,
                    output: None,
                    exit_code: None,
                }
            }
            loom_provider_protocol::ScriptRunOutcome::Cancelled { output, .. } => {
                AutomationRunOutcome::Cancelled {
                    reason: format!("the host stopped the script: {output}"),
                }
            }
            loom_provider_protocol::ScriptRunOutcome::Exited {
                exit_code,
                output,
                output_truncated,
                timed_out,
                script_path,
            } => {
                let output = (!output.is_empty()).then(|| {
                    if output_truncated {
                        // The record says the output is a prefix of what the
                        // script printed, so nobody reads a cut log as a whole
                        // one.
                        format!("{output}\n[output truncated]")
                    } else {
                        output
                    }
                });
                if let Some(path) = script_path {
                    // The host wrote an inline script somewhere only it knows;
                    // recording the path is what lets a client show where the
                    // code that ran lives.
                    self.automations
                        .record_stored_script_path(&report.run_id, &path, now);
                }
                match (timed_out, exit_code) {
                    (true, _) => AutomationRunOutcome::Failed {
                        error: "Script timed out".to_owned(),
                        thread_id: None,
                        output,
                        exit_code: None,
                    },
                    (false, Some(0)) if output_is_empty(&output) => {
                        // Upstream's rule, kept: a script that printed nothing
                        // has not failed, it has nothing to say.
                        AutomationRunOutcome::Skipped {
                            reason: "empty output".to_owned(),
                            exit_code: Some(0),
                        }
                    }
                    (false, Some(0)) if output.as_deref().is_some_and(suppresses_wake_agent) => {
                        AutomationRunOutcome::Skipped {
                            reason: "wakeAgent false".to_owned(),
                            exit_code: Some(0),
                        }
                    }
                    (false, Some(0)) => AutomationRunOutcome::Succeeded {
                        thread_id: None,
                        output,
                        exit_code: Some(0),
                    },
                    (false, Some(code)) => AutomationRunOutcome::Failed {
                        error: format!("Script exited with code {code}"),
                        thread_id: None,
                        output,
                        exit_code: Some(code),
                    },
                    (false, None) => AutomationRunOutcome::Failed {
                        error: "Script was terminated before it exited".to_owned(),
                        thread_id: None,
                        output,
                        exit_code: None,
                    },
                }
            }
        };
        match self.automations.close_run(&report.run_id, &outcome, now) {
            Ok(closed) => {
                eprintln!(
                    "loom-server: automation run {} ({}) {} after a script report",
                    closed.id,
                    closed.automation_id,
                    closed.state.as_str()
                );
                if let Some(project_id) = self.automations.project_of_run(&closed.id) {
                    self.publish_automations_changed(&project_id.to_string());
                }
                ScriptReportOutcome::Applied
            }
            Err(error) => {
                eprintln!(
                    "loom-server: could not close automation run {}: {error}",
                    report.run_id
                );
                ScriptReportOutcome::Stale
            }
        }
    }

    /// Stops the script runs of one automation and settles them as cancelled.
    ///
    /// This is what pausing and deleting reach: an agent run cannot be
    /// interrupted (the provider protocol has no cancel frame), but a script is
    /// a process on a known host, so the host is told to kill it. The run is
    /// settled here rather than waiting for the host: the user asked for it to
    /// stop, and the report that follows finds nothing in flight — which is
    /// exactly how a cancel race stays harmless.
    pub fn cancel_script_runs(
        &self,
        automation_id: &str,
        reason: &str,
        now: u64,
    ) -> Vec<AutomationRun> {
        let mut cancelled = Vec::new();
        for run in self.automations.running_script_runs() {
            if run.automation_id.to_string() != automation_id {
                continue;
            }
            let Some(host_id) = run.host_id.clone() else {
                continue;
            };
            let cancel = loom_provider_protocol::ScriptRunCancel {
                run_id: run.id.clone(),
                host_id: host_id.clone(),
                reason: reason.to_owned(),
                created_at_ms: now,
            };
            let payload = serde_json::to_vec(&cancel).expect("a cancel always serializes");
            if let Err(error) = self.publish(loom_relay::Scope::Host(host_id.to_string()), payload)
            {
                eprintln!(
                    "loom-server: the cancel for automation run {} could not be published: {error}",
                    run.id
                );
            }
            let outcome = AutomationRunOutcome::Cancelled {
                reason: reason.to_owned(),
            };
            match self.automations.close_run(&run.id, &outcome, now) {
                Ok(closed) => {
                    if let Some(project_id) = self.automations.project_of_run(&closed.id) {
                        self.publish_automations_changed(&project_id.to_string());
                    }
                    cancelled.push(closed);
                }
                Err(error) => eprintln!(
                    "loom-server: could not settle cancelled automation run {}: {error}",
                    run.id
                ),
            }
        }
        cancelled
    }

    /// Fails script runs whose host can no longer be trusted to report.
    ///
    /// A script run is a process on a machine. If that machine is no longer
    /// connected, or if it never reported within its own timeout plus a
    /// generous transit margin, the run is not going to end by itself — and a
    /// run left in flight holds its automation's single-flight slot forever.
    /// Failing it is the same argument the provider reaper makes.
    pub fn reconcile_script_runs(&self, now: u64) -> usize {
        let mut failed = 0;
        for run in self.automations.running_script_runs() {
            let reason = match run.host_id.clone() {
                None => Some("the script run has no host to report it".to_owned()),
                Some(host_id) => match self.registry.host(&host_id) {
                    None => Some(format!("host {host_id} is no longer enrolled")),
                    Some(host) if host.status != loom_domain::HostStatus::Connected => {
                        Some(format!(
                        "host {host_id} is no longer connected, so its script run cannot report"
                    ))
                    }
                    Some(_) => {
                        let timeout = self
                            .automations
                            .automation(&run.automation_id)
                            .and_then(|automation| match automation.execution {
                                AutomationExecution::Script(script) => Some(script.timeout_ms),
                                AutomationExecution::Agent(_) => None,
                            })
                            .unwrap_or(loom_domain::automation::AUTOMATION_SCRIPT_TIMEOUT_MAX_MS);
                        let deadline = run
                            .started_at
                            .saturating_add(timeout)
                            // Transit and reporting margin: the host enforces
                            // the timeout itself, so this only catches a report
                            // that never came.
                            .saturating_add(
                                loom_domain::automation::AUTOMATION_SCRIPT_TIMEOUT_DEFAULT_MS,
                            );
                        (now > deadline).then(|| {
                            format!(
                                "the host did not report the script run within {}ms of its \
                                 timeout",
                                timeout
                            )
                        })
                    }
                },
            };
            let Some(reason) = reason else {
                continue;
            };
            let outcome = AutomationRunOutcome::Failed {
                error: reason.clone(),
                thread_id: None,
                output: None,
                exit_code: None,
            };
            if self.automations.close_run(&run.id, &outcome, now).is_ok() {
                eprintln!("loom-server: automation run {} failed: {reason}", run.id);
                if let Some(project_id) = self.automations.project_of_run(&run.id) {
                    self.publish_automations_changed(&project_id.to_string());
                }
                failed += 1;
            }
        }
        failed
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
            Ok(closed) => {
                eprintln!(
                    "loom-server: automation run {} ({}) failed before dispatch: {}",
                    closed.id,
                    closed.automation_id,
                    closed.error.unwrap_or_default()
                );
                if let Some(project_id) = self.automations.project_of_run(&closed.id) {
                    self.publish_automations_changed(&project_id.to_string());
                }
            }
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
    use crate::state::realtime_test_support::expect_project_invalidation;
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
            entity_write_interval: Duration::ZERO,
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
                reasoning_level: ReasoningLevel::from("medium"),
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
        // `working`. The turn's own events come from the worker: the server
        // publishes the dispatch to the host's scope and waits.
        assert_eq!(
            thread_timeline(&state, &thread_id),
            vec!["thread_message_added", "thread_status_changed"],
            "the message and the transition to working, and nothing the worker has not reported"
        );

        // The mapping is durable in both directions.
        let mark = state
            .automations
            .thread_mark(&thread_id)
            .expect("the thread is marked as automation-produced");
        assert_eq!(mark.automation_id, automation.id);
        assert_eq!(mark.run_id, queued.id);
        state.shutdown().unwrap();
    }

    #[tokio::test]
    async fn the_provider_run_ending_closes_the_automation_run_it_belongs_to() {
        let state = state();
        let mut events = state.public_events.subscribe();
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
        // The agent path settles through the provider's report rather than the
        // script one, and it publishes the same project invalidation: a client
        // rendering the automation's history is told the same way either way.
        expect_project_invalidation(&mut events, &project.to_string()).await;
        state.shutdown().unwrap();
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
        state.shutdown().unwrap();
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
        state.shutdown().unwrap();
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
        state.shutdown().unwrap();
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
        state.shutdown().unwrap();
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
        state.shutdown().unwrap();
    }

    /// A script automation in `project`.
    fn script_automation(state: &AppState, project: &ProjectId, name: &str) -> Automation {
        use loom_domain::automation::{AutomationExecution, ScriptExecution, ScriptInterpreter};
        let new = NewAutomation {
            name: name.into(),
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
                stored_script_path: None,
            }),
            origin: AutomationOrigin::Human,
            created_by_thread_id: None,
        };
        state
            .automations
            .create(project.clone(), new, now_ms())
            .expect("creates")
    }

    #[tokio::test]
    async fn a_script_run_with_no_machine_fails_with_the_reason() {
        let state = state();
        let project = state.registry.personal_project_id();
        let automation = script_automation(&state, &project, "backup");
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
            .is_some_and(|error| error.contains("no connected machine")));
        state.shutdown().unwrap();
    }

    #[tokio::test]
    async fn a_script_run_on_a_machine_without_a_reported_data_directory_fails() {
        let state = state();
        // The workspace of a script lives under the machine's data directory,
        // which the machine reports when it enrolls. A machine that never
        // reported one cannot be handed a script, and the run says so.
        let (_host, events) = state
            .registry
            .enroll_host(None, "worker".into(), now_ms())
            .expect("enrolls");
        for event in &events {
            state.publish_domain_event(event).expect("publishes");
        }
        let project = state.registry.personal_project_id();
        let automation = script_automation(&state, &project, "backup");
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
        assert!(
            run.error
                .as_deref()
                .is_some_and(|error| error.contains("has not reported a data directory")),
            "{:?}",
            run.error
        );
        assert!(run.host_id.is_none());
        state.shutdown().unwrap();
    }

    #[tokio::test]
    async fn a_script_run_is_dispatched_to_the_machine_that_owns_it() {
        let state = state();
        // A script runs in the machine's own script directory, so the machine
        // has to have reported where that is.
        let (host, events) = state
            .registry
            .enroll_host_with_data_dir(
                None,
                "worker".into(),
                Some("/var/lib/loom".into()),
                now_ms(),
            )
            .expect("enrolls");
        for event in &events {
            state.publish_domain_event(event).expect("publishes");
        }
        let host_id = host.id;
        let project = state.registry.personal_project_id();
        let automation = script_automation(&state, &project, "backup");
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
        assert_eq!(report.dispatched, 1, "{report:?}");
        let run = state.automations.run(&queued.id).expect("stored");
        assert_eq!(run.state, AutomationRunState::Running);
        assert_eq!(run.host_id, Some(host_id.clone()));
        assert!(run.thread_id.is_none(), "a script run has no thread");

        // What the machine receives is the script, its interpreter, its
        // workspace and the timeout — everything it needs without calling back.
        let frames = state
            .relay
            .replay_scope(&Scope::Host(host_id.to_string()), 50)
            .expect("replays");
        let dispatch = frames
            .iter()
            .filter_map(|frame| {
                let value: serde_json::Value = serde_json::from_slice(&frame.payload).ok()?;
                let payload = value["payload"].as_str()?;
                serde_json::from_str::<loom_provider_protocol::ScriptRunDispatch>(payload).ok()
            })
            .next()
            .expect("the dispatch reached the host scope");
        assert_eq!(dispatch.run_id, queued.id);
        assert_eq!(dispatch.script.as_deref(), Some("echo hi"));
        assert_eq!(
            dispatch.cwd,
            format!("/var/lib/loom/automation-scripts/{}", automation.id),
            "the workspace is the machine's own script directory"
        );
        assert_eq!(
            dispatch.timeout_ms,
            loom_domain::automation::AUTOMATION_SCRIPT_TIMEOUT_DEFAULT_MS
        );
        state.shutdown().unwrap();
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
                reasoning_level: ReasoningLevel::from("medium"),
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
        state.shutdown().unwrap();
    }

    /// Dispatches one script run to an enrolled host.
    ///
    /// Returns the host the *dispatch* chose, read back from the run, rather
    /// than the one enrolled here: a script goes to the primary host, and a test
    /// that enrolls several (one per phase) must report from whichever host the
    /// control plane actually picked, or it fails on ordering it does not
    /// control.
    async fn dispatched_script_run(
        state: &AppState,
        name: &str,
    ) -> (loom_domain::HostId, AutomationRun) {
        let (_host, events) = state
            .registry
            .enroll_host_with_data_dir(
                None,
                "worker".into(),
                Some("/var/lib/loom".into()),
                now_ms(),
            )
            .expect("enrolls");
        for event in &events {
            state.publish_domain_event(event).expect("publishes");
        }
        let project = state.registry.personal_project_id();
        let automation = script_automation(state, &project, name);
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
        assert_eq!(report.dispatched, 1, "{report:?}");
        let run = state.automations.run(&queued.id).expect("stored");
        let dispatched_to = run.host_id.clone().expect("the dispatch recorded its host");
        (dispatched_to, run)
    }

    /// Applies one exit report the way the worker's socket would.
    fn report_exited(
        state: &AppState,
        host: &loom_domain::HostId,
        run: &AutomationRun,
        exit_code: Option<i32>,
        output: &str,
        timed_out: bool,
    ) -> ScriptReportOutcome {
        state.apply_script_run_report(
            host,
            loom_provider_protocol::ScriptRunReport {
                host_id: host.clone(),
                run_id: run.id.clone(),
                outcome: loom_provider_protocol::ScriptRunOutcome::Exited {
                    exit_code,
                    output: output.to_owned(),
                    output_truncated: false,
                    timed_out,
                    script_path: None,
                },
            },
        )
    }

    #[tokio::test]
    async fn a_script_report_is_the_run_result_and_a_repeat_is_stale() {
        let state = state();

        // A script that printed something succeeded and kept its output.
        let (host, run) = dispatched_script_run(&state, "printed").await;
        assert_eq!(
            report_exited(&state, &host, &run, Some(0), "hello\n", false),
            ScriptReportOutcome::Applied
        );
        let settled = state.automations.run(&run.id).expect("stored");
        assert_eq!(settled.state, AutomationRunState::Succeeded);
        assert_eq!(settled.output.as_deref(), Some("hello\n"));
        assert_eq!(settled.exit_code, Some(0));

        // The same report again — a redelivery — changes nothing.
        assert_eq!(
            report_exited(&state, &host, &run, Some(0), "hello\n", false),
            ScriptReportOutcome::Stale
        );

        // A non-zero exit failed the run and kept what it printed.
        let (host, run) = dispatched_script_run(&state, "failed").await;
        assert_eq!(
            report_exited(&state, &host, &run, Some(2), "bad\n", false),
            ScriptReportOutcome::Applied
        );
        let settled = state.automations.run(&run.id).expect("stored");
        assert_eq!(settled.state, AutomationRunState::Failed);
        assert_eq!(settled.error.as_deref(), Some("Script exited with code 2"));
        assert_eq!(settled.output.as_deref(), Some("bad\n"));
        assert_eq!(settled.exit_code, Some(2));

        // A timeout is a failure with its own words.
        let (host, run) = dispatched_script_run(&state, "timeout").await;
        assert_eq!(
            report_exited(&state, &host, &run, None, "", true),
            ScriptReportOutcome::Applied
        );
        let settled = state.automations.run(&run.id).expect("stored");
        assert_eq!(settled.state, AutomationRunState::Failed);
        assert_eq!(settled.error.as_deref(), Some("Script timed out"));
        state.shutdown().unwrap();
    }

    #[tokio::test]
    async fn a_silent_script_is_skipped_the_way_the_reference_records_it() {
        let state = state();

        // Whitespace only: the run had nothing to say, so it is skipped rather
        // than succeeded — with the exit code it did have.
        let (host, run) = dispatched_script_run(&state, "silent").await;
        assert_eq!(
            report_exited(&state, &host, &run, Some(0), "  \n\n", false),
            ScriptReportOutcome::Applied
        );
        let settled = state.automations.run(&run.id).expect("stored");
        assert_eq!(settled.state, AutomationRunState::Skipped);
        assert_eq!(settled.skip_reason.as_deref(), Some("empty output"));
        assert_eq!(settled.exit_code, Some(0));
        assert!(settled.output.is_none());

        // A trailing `{"wakeAgent": false}` reads as the same "nothing to do"
        // even though the script printed something.
        let (host, run) = dispatched_script_run(&state, "quiet").await;
        assert_eq!(
            report_exited(
                &state,
                &host,
                &run,
                Some(0),
                "worked\n{\"wakeAgent\": false}\n",
                false
            ),
            ScriptReportOutcome::Applied
        );
        let settled = state.automations.run(&run.id).expect("stored");
        assert_eq!(settled.state, AutomationRunState::Skipped);
        assert_eq!(settled.skip_reason.as_deref(), Some("wakeAgent false"));

        // `true` is not a suppression: the run succeeded and kept the output.
        let (host, run) = dispatched_script_run(&state, "loud").await;
        assert_eq!(
            report_exited(
                &state,
                &host,
                &run,
                Some(0),
                "worked\n{\"wakeAgent\": true}\n",
                false
            ),
            ScriptReportOutcome::Applied
        );
        let settled = state.automations.run(&run.id).expect("stored");
        assert_eq!(settled.state, AutomationRunState::Succeeded);
        state.shutdown().unwrap();
    }

    #[tokio::test]
    async fn a_settled_script_run_invalidates_its_project() {
        let state = state();
        let mut events = state.public_events.subscribe();
        let (host, run) = dispatched_script_run(&state, "reported").await;

        assert_eq!(
            report_exited(&state, &host, &run, Some(0), "done\n", false),
            ScriptReportOutcome::Applied
        );
        let project = state.registry.personal_project_id().to_string();
        expect_project_invalidation(&mut events, &project).await;
        state.shutdown().unwrap();
    }

    #[tokio::test]
    async fn cancelling_a_script_run_invalidates_its_project() {
        let state = state();
        let mut events = state.public_events.subscribe();
        let (host, run) = dispatched_script_run(&state, "cancelled").await;

        let cancelled = state.cancel_script_runs(
            &run.automation_id.to_string(),
            "cancelled: the automation was paused",
            now_ms(),
        );
        assert_eq!(cancelled.len(), 1, "the running script should be settled");
        let project = state.registry.personal_project_id().to_string();
        expect_project_invalidation(&mut events, &project).await;
        assert_eq!(run.host_id, Some(host));
        state.shutdown().unwrap();
    }

    #[tokio::test]
    async fn a_run_that_cannot_be_dispatched_invalidates_its_project() {
        let state = state();
        let mut events = state.public_events.subscribe();
        // No machine is enrolled, so the queued script cannot be handed to
        // anyone: the settle is what a client has to see.
        let project = state.registry.personal_project_id();
        let automation = script_automation(&state, &project, "nowhere");
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
        assert_eq!(
            queued.run_mode,
            loom_domain::automation::AutomationRunMode::Script
        );
        expect_project_invalidation(&mut events, &project.to_string()).await;
        state.shutdown().unwrap();
    }

    #[tokio::test]
    async fn a_script_report_from_another_host_is_refused() {
        let state = state();
        let (host, run) = dispatched_script_run(&state, "owned").await;
        let (other, events) = state
            .registry
            .enroll_host(None, "intruder".into(), now_ms())
            .expect("enrolls");
        for event in &events {
            state.publish_domain_event(event).expect("publishes");
        }

        let outcome = state.apply_script_run_report(
            &other.id,
            loom_provider_protocol::ScriptRunReport {
                host_id: other.id.clone(),
                run_id: run.id.clone(),
                outcome: loom_provider_protocol::ScriptRunOutcome::Exited {
                    exit_code: Some(0),
                    output: "not mine".into(),
                    output_truncated: false,
                    timed_out: false,
                    script_path: None,
                },
            },
        );
        assert!(
            matches!(outcome, ScriptReportOutcome::Mismatch(_)),
            "{outcome:?}"
        );
        // The run the dispatch went to is untouched and still in flight.
        let stored = state.automations.run(&run.id).expect("stored");
        assert_eq!(stored.state, AutomationRunState::Running);
        assert_eq!(stored.host_id, Some(host));
        state.shutdown().unwrap();
    }
}
