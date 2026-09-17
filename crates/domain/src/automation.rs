//! Automations: a project-owned trigger that starts an agent or script run.
//!
//! An [`Automation`] belongs to exactly one [`Project`](crate::Project) and
//! carries two halves: a [`AutomationTrigger`] (a cron `schedule` or a single
//! `once` instant) and an [`AutomationExecution`] (an `agent` run with its
//! provider/model/permission/environment selection, or a `script` the owning
//! host runs). Every run it produces is an [`AutomationRun`] row in its
//! history, and [`AutomationThreadMark`] keeps the mapping from a thread back
//! to the automation and run that produced it.
//!
//! # Scope of this type today
//!
//! The types here are the whole **contract** for automations — the shapes the
//! HTTP surface projects — but they deliberately say nothing about *when* a
//! trigger fires or *how* an execution is carried out. Those are the
//! scheduler's and the execution plane's jobs; this module owns invariants
//! only:
//!
//! * [`AutomationTrigger::next_run_at_after`] answers "when is this due next"
//!   for a `once` trigger, and returns `None` for a `schedule` because
//!   evaluating a cron expression in a timezone needs a scheduler that does
//!   not exist yet. A schedule's `nextRunAt` stays `null` until then rather
//!   than being filled with a time the server would not honour.
//! * [`Automation::start_manual_run`] creates the history row a manual trigger
//!   produces and returns it, without starting anything: execution is the next
//!   stage's, and a run this server created stays `running` until the execution
//!   plane reports a terminal state for it.
//!
//! # Validation
//!
//! Everything a client can send is validated here, with the same limits the
//! contract's `limits.ts` declares: names, script bodies and paths, cron and
//! timezone syntax, one-shot instants in the future, exactly one script
//! source, and the run-list page bounds. A rejection is a
//! [`DomainError::InvalidField`], which the HTTP surface reports as
//! `400 invalid_request`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::error::DomainError;
use crate::id::{AutomationId, AutomationRunId, EnvironmentId, HostId, ProjectId, ThreadId};
use crate::queue::ServiceTier;
use crate::thread::ReasoningLevel;

/// Longest accepted automation name (`AUTOMATION_NAME_MAX_LENGTH`).
pub const AUTOMATION_NAME_MAX_LENGTH: usize = 200;
/// Longest accepted inline script body (`AUTOMATION_SCRIPT_MAX_LENGTH`).
pub const AUTOMATION_SCRIPT_MAX_LENGTH: usize = 262_144;
/// Longest accepted script path (`AUTOMATION_SCRIPT_FILE_MAX_LENGTH`).
pub const AUTOMATION_SCRIPT_FILE_MAX_LENGTH: usize = 200;
/// Longest accepted cron expression (`SCHEDULE_CRON_MAX_LENGTH`).
pub const SCHEDULE_CRON_MAX_LENGTH: usize = 100;
/// Longest accepted timezone name (`SCHEDULE_TIMEZONE_MAX_LENGTH`).
pub const SCHEDULE_TIMEZONE_MAX_LENGTH: usize = 100;
/// Longest accepted manual-run idempotency key.
pub const AUTOMATION_IDEMPOTENCY_KEY_MAX_LENGTH: usize = 200;
/// Script timeout used when a client does not choose one.
pub const AUTOMATION_SCRIPT_TIMEOUT_DEFAULT_MS: u64 = 120_000;
/// Longest accepted script timeout.
pub const AUTOMATION_SCRIPT_TIMEOUT_MAX_MS: u64 = 900_000;
/// Run-list page size used when a client does not choose one.
pub const AUTOMATION_RUNS_LIMIT_DEFAULT: u32 = 50;
/// Largest run-list page size (`AUTOMATION_RUNS_LIMIT_MAX`).
pub const AUTOMATION_RUNS_LIMIT_MAX: u32 = 200;

/// The permission policy an automation's agent run requests.
///
/// The same three tokens as bb's `permissionModeSchema`. It is deliberately
/// its own type rather than a reuse of
/// [`HostPermissionMode`](crate::HostPermissionMode): a host ceiling and an
/// automation's request are different decisions that happen to share a
/// vocabulary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PermissionMode {
    /// The provider may edit files, but not use unrestricted access.
    AcceptEdits,
    /// The provider may use the normal automatic policy.
    Auto,
    /// The provider may use all ACP capabilities.
    Full,
}

impl PermissionMode {
    /// The contract spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AcceptEdits => "accept-edits",
            Self::Auto => "auto",
            Self::Full => "full",
        }
    }
}

/// Who created the automation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AutomationOrigin {
    /// A person, through a client.
    Human,
    /// A client application acting on its own.
    App,
    /// Another agent's thread.
    Agent,
}

/// What a run of this automation will execute.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AutomationRunMode {
    /// A provider turn in a thread.
    Agent,
    /// A script on the owning host.
    Script,
}

/// Where a run is in its lifecycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AutomationRunStatus {
    /// In flight. Nothing but the execution plane may leave it.
    Running,
    /// Finished successfully.
    Succeeded,
    /// Finished with an error.
    Failed,
    /// Deliberately not executed, with a [`AutomationRun::skip_reason`].
    Skipped,
}

/// What asked for the run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AutomationRunTrigger {
    /// The trigger's schedule fired.
    Schedule,
    /// A client asked for it with `automations_run`.
    Manual,
}

/// The interpreter a script run uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScriptInterpreter {
    /// `bash`.
    Bash,
    /// `sh`.
    Sh,
    /// `node`.
    Node,
    /// `python3`.
    #[serde(rename = "python3")]
    Python3,
}

/// When an automation runs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "triggerType",
    rename_all = "lowercase",
    rename_all_fields = "camelCase"
)]
pub enum AutomationTrigger {
    /// Repeats on a five-field cron expression in a named timezone.
    Schedule {
        /// The cron expression, `minute hour day-of-month month day-of-week`.
        cron: String,
        /// The IANA timezone name the expression is read in.
        timezone: String,
    },
    /// Fires once at an absolute instant.
    Once {
        /// Epoch milliseconds the run is due at.
        run_at: u64,
    },
}

impl AutomationTrigger {
    /// The contract's `triggerType` token.
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Schedule { .. } => "schedule",
            Self::Once { .. } => "once",
        }
    }

    /// Validates the trigger against the clock.
    ///
    /// A `once` trigger must be in the future: a definition that is already due
    /// is rejected rather than turned into an immediate run, because "fire now"
    /// is what the manual `run` operation is for and silently converting one
    /// into the other would hide a client's stale timestamp.
    pub fn validate(&self, now_ms: u64) -> Result<(), DomainError> {
        match self {
            Self::Schedule { cron, timezone } => {
                validate_cron(cron)?;
                validate_timezone(timezone)
            }
            Self::Once { run_at } => {
                if *run_at <= now_ms {
                    return Err(invalid(
                        "trigger.runAt",
                        "must be in the future for a one-shot automation",
                    ));
                }
                Ok(())
            }
        }
    }

    /// The instant this trigger is next due at, once `enabled` is applied.
    ///
    /// `None` means "not scheduled", which covers both a paused automation and
    /// a cron schedule this build cannot evaluate: the field is the scheduler's
    /// answer and a guess would be indistinguishable from a real one. A `once`
    /// trigger is its own answer, so it is reported exactly.
    pub fn next_run_at_after(&self, enabled: bool, _now_ms: u64) -> Option<u64> {
        match self {
            Self::Schedule { .. } => None,
            Self::Once { run_at } => enabled.then_some(*run_at),
        }
    }
}

/// Cuts a personal workspace from a branch, or checks out a declared path.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub enum UnmanagedBranchSpec {
    /// Use a branch that already exists.
    Existing {
        /// Branch name.
        name: String,
    },
    /// Create a branch from another one.
    New {
        /// The branch to start from.
        base_branch: String,
    },
}

/// The branch a managed worktree is cut from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "lowercase",
    rename_all_fields = "camelCase"
)]
pub enum ManagedBaseBranch {
    /// A named branch.
    Named {
        /// Branch name.
        name: String,
    },
    /// The repository's default branch.
    Default,
}

/// Where an agent execution's workspace comes from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub enum WorkspaceKind {
    /// A workspace the client manages: a path, optionally on a branch.
    Unmanaged {
        /// Absolute path on the host. Absent and `null` are the same thing to
        /// this layer: the response always writes the key.
        ///
        /// The key is *required* by the contract
        /// (`z.string().min(1).nullable()`), which `skip_serializing_if` would
        /// violate by dropping it whenever no path is set — so `null` is
        /// written instead of the key going missing.
        #[serde(default)]
        path: Option<String>,
        /// The branch the workspace should be on.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        branch: Option<UnmanagedBranchSpec>,
    },
    /// A worktree loom manages for the run.
    ManagedWorktree {
        /// The branch to cut it from.
        base_branch: ManagedBaseBranch,
    },
    /// The caller's personal workspace.
    Personal,
}

/// The environment an agent execution runs in.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub enum AgentEnvironment {
    /// An environment that already exists.
    Reuse {
        /// The environment to reuse.
        environment_id: EnvironmentId,
    },
    /// A workspace on an enrolled host.
    Host {
        /// The host the workspace lives on. Required unless the workspace is
        /// personal, which is the one case the server can resolve without a
        /// machine being named.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        host_id: Option<HostId>,
        /// The workspace to use.
        workspace: WorkspaceKind,
    },
    /// Whatever the owning project provisions by default.
    ProjectDefault,
}

impl AgentEnvironment {
    /// Validates the environment's own coherence.
    pub fn validate(&self) -> Result<(), DomainError> {
        if let Self::Host { host_id, workspace } = self {
            if host_id.is_none() && workspace != &WorkspaceKind::Personal {
                return Err(invalid(
                    "execution.environment.hostId",
                    "is required unless workspace.type is personal",
                ));
            }
        }
        Ok(())
    }
}

/// An agent run's execution definition.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AgentExecution {
    /// What the run is asked to do.
    pub prompt: String,
    /// The provider that serves the run.
    pub provider_id: String,
    /// The model the provider is asked for.
    pub model: String,
    /// How much reasoning the run asks for.
    #[serde(default = "default_reasoning_level")]
    pub reasoning_level: ReasoningLevel,
    /// The service tier the run asks for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<ServiceTier>,
    /// The permission policy the run requests.
    pub permission_mode: PermissionMode,
    /// Where the run works.
    pub environment: AgentEnvironment,
    /// An existing thread to send the run to instead of starting a new one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_thread_id: Option<ThreadId>,
}

fn default_reasoning_level() -> ReasoningLevel {
    ReasoningLevel::Medium
}

impl AgentExecution {
    /// Validates the fields the contract requires to be non-empty, plus the
    /// environment's own coherence.
    pub fn validate(&self) -> Result<(), DomainError> {
        validate_non_empty("execution.prompt", &self.prompt)?;
        self.validate_stored()
    }

    /// Everything a stored row must still satisfy — which is [`validate`]
    /// without the prompt rule.
    ///
    /// The one difference is deliberate: a row written by a build that allowed
    /// an empty prompt must stay *readable*, so it is decoded and reported as
    /// `missing-agent-prompt` rather than as unreadable data. Everything else
    /// is canonical on both paths.
    pub fn validate_stored(&self) -> Result<(), DomainError> {
        validate_non_empty("execution.providerId", &self.provider_id)?;
        validate_non_empty("execution.model", &self.model)?;
        self.environment.validate()
    }

    /// Whether this execution still carries a legacy empty prompt.
    ///
    /// It parses — a stored row from a build that allowed it must stay
    /// readable — but it cannot be run, paused, resumed, updated or scheduled,
    /// which the HTTP surface reports as a read problem rather than as a
    /// deletion.
    pub fn is_missing_prompt(&self) -> bool {
        self.prompt.is_empty()
    }
}

/// A script run's execution definition.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ScriptExecution {
    /// The script body, inline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script: Option<String>,
    /// A path to the script on the owning host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script_file: Option<String>,
    /// The interpreter to run it with. Absent means the host's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interpreter: Option<ScriptInterpreter>,
    /// How long the run may take.
    #[serde(default = "default_script_timeout_ms")]
    pub timeout_ms: u64,
    /// Extra environment variables for the run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<BTreeMap<String, String>>,
}

fn default_script_timeout_ms() -> u64 {
    AUTOMATION_SCRIPT_TIMEOUT_DEFAULT_MS
}

impl ScriptExecution {
    /// Validates the source and bounds.
    pub fn validate(&self) -> Result<(), DomainError> {
        match (self.script.as_deref(), self.script_file.as_deref()) {
            (Some(script), None) => {
                if script.is_empty() {
                    return Err(invalid("execution.script", "must not be empty"));
                }
                if script.chars().count() > AUTOMATION_SCRIPT_MAX_LENGTH {
                    return Err(invalid(
                        "execution.script",
                        format!("must be at most {AUTOMATION_SCRIPT_MAX_LENGTH} characters"),
                    ));
                }
            }
            (None, Some(path)) => {
                if path.is_empty() {
                    return Err(invalid("execution.scriptFile", "must not be empty"));
                }
                if path.chars().count() > AUTOMATION_SCRIPT_FILE_MAX_LENGTH {
                    return Err(invalid(
                        "execution.scriptFile",
                        format!("must be at most {AUTOMATION_SCRIPT_FILE_MAX_LENGTH} characters"),
                    ));
                }
            }
            _ => {
                return Err(invalid(
                    "execution.script",
                    "provide exactly one of script | scriptFile",
                ))
            }
        }
        if self.timeout_ms == 0 || self.timeout_ms > AUTOMATION_SCRIPT_TIMEOUT_MAX_MS {
            return Err(invalid(
                "execution.timeoutMs",
                format!("must be between 1 and {AUTOMATION_SCRIPT_TIMEOUT_MAX_MS}"),
            ));
        }
        Ok(())
    }
}

/// What a run executes, discriminated by `mode`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "lowercase")]
pub enum AutomationExecution {
    /// A provider turn.
    Agent(AgentExecution),
    /// A host script.
    Script(ScriptExecution),
}

impl AutomationExecution {
    /// Validates whichever half is active.
    pub fn validate(&self) -> Result<(), DomainError> {
        match self {
            Self::Agent(agent) => agent.validate(),
            Self::Script(script) => script.validate(),
        }
    }

    /// Validates whichever half is active, without the agent prompt rule.
    ///
    /// Decoding a stored row uses this: an empty prompt is a *read problem*,
    /// while every other violation makes the row unreadable.
    pub fn validate_stored(&self) -> Result<(), DomainError> {
        match self {
            Self::Agent(agent) => agent.validate_stored(),
            Self::Script(script) => script.validate(),
        }
    }

    /// The contract's `mode` token, which is also the stored `runMode`.
    pub const fn run_mode(&self) -> AutomationRunMode {
        match self {
            Self::Agent(_) => AutomationRunMode::Agent,
            Self::Script(_) => AutomationRunMode::Script,
        }
    }

    /// The thread this execution targets, when it targets one.
    pub fn target_thread_id(&self) -> Option<&ThreadId> {
        match self {
            Self::Agent(agent) => agent.target_thread_id.as_ref(),
            Self::Script(_) => None,
        }
    }

    /// Whether an agent execution is missing its prompt.
    pub fn is_missing_prompt(&self) -> bool {
        matches!(self, Self::Agent(agent) if agent.is_missing_prompt())
    }
}

/// Where an [`agent` update](AutomationUpdate::agent) sends future runs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub enum AgentExecutionTarget {
    /// Reuse an existing thread.
    TargetThread {
        /// The thread to send runs to.
        thread_id: ThreadId,
    },
    /// Run in a freshly resolved environment.
    Environment {
        /// The environment to run in.
        environment: AgentEnvironment,
    },
}

/// A partial update of an agent execution.
///
/// `serviceTier: null` clears the tier, which is why it is an
/// `Option<Option<_>>`: absent keeps the stored value.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AgentExecutionUpdate {
    /// New prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// New provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_id: Option<String>,
    /// New model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// New reasoning level.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_level: Option<ReasoningLevel>,
    /// New service tier; `null` removes the stored one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<Option<ServiceTier>>,
    /// New permission mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<PermissionMode>,
    /// Where future runs go.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<AgentExecutionTarget>,
}

impl AgentExecutionUpdate {
    /// Whether the patch names at least one field.
    pub fn is_empty(&self) -> bool {
        self.prompt.is_none()
            && self.provider_id.is_none()
            && self.model.is_none()
            && self.reasoning_level.is_none()
            && self.service_tier.is_none()
            && self.permission_mode.is_none()
            && self.target.is_none()
    }
}

/// A partial update of an automation.
///
/// The three field groups are merged by [`Automation::update`], which is why
/// the request validation for "at least one field" and "execution and agent
/// are mutually exclusive" lives there too: they are properties of the update
/// as a whole, not of any one field.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AutomationUpdate {
    /// New name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// A replacement trigger.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger: Option<AutomationTrigger>,
    /// A replacement execution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution: Option<AutomationExecution>,
    /// A patch applied to the stored agent execution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentExecutionUpdate>,
}

impl AutomationUpdate {
    /// Whether the patch names at least one field.
    pub fn is_empty(&self) -> bool {
        self.name.is_none()
            && self.trigger.is_none()
            && self.execution.is_none()
            && self.agent.is_none()
    }
}

/// The definition a create request carries.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct NewAutomation {
    /// Display name.
    pub name: String,
    /// Whether the trigger is live right away.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// What starts a run.
    pub trigger: AutomationTrigger,
    /// What a run does.
    pub execution: AutomationExecution,
    /// Who asked for it.
    pub origin: AutomationOrigin,
    /// The thread that created it, when an agent did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by_thread_id: Option<ThreadId>,
}

fn default_enabled() -> bool {
    true
}

/// A trigger plus the execution it starts, owned by a project.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Automation {
    /// Identity.
    pub id: AutomationId,
    /// The owning project.
    pub project_id: ProjectId,
    /// Display name.
    pub name: String,
    /// Whether the trigger is live.
    pub enabled: bool,
    /// What starts a run.
    pub trigger: AutomationTrigger,
    /// What a run does.
    pub execution: AutomationExecution,
    /// Who asked for it.
    pub origin: AutomationOrigin,
    /// The thread that created it, when an agent did.
    pub created_by_thread_id: Option<ThreadId>,
    /// When the next run is due, when that is known.
    pub next_run_at: Option<u64>,
    /// When a run last started.
    pub last_run_at: Option<u64>,
    /// How many scheduled runs have started.
    pub run_count: u64,
    /// How many runs failed in a row.
    pub consecutive_failures: u32,
    /// The status of the last finished run.
    pub last_run_status: Option<AutomationRunStatus>,
    /// The thread the last run produced or was sent to.
    pub last_run_thread_id: Option<ThreadId>,
    /// Why the last run failed, when it did.
    pub last_error: Option<String>,
    /// Wall-clock milliseconds when it was created.
    pub created_at_ms: u64,
    /// Wall-clock milliseconds of the last mutation.
    pub updated_at_ms: u64,
}

impl Automation {
    /// Validates and creates an automation.
    pub fn create(
        id: AutomationId,
        project_id: ProjectId,
        new: NewAutomation,
        now_ms: u64,
    ) -> Result<Self, DomainError> {
        validate_name(&new.name)?;
        new.trigger.validate(now_ms)?;
        new.execution.validate()?;
        let next_run_at = new.trigger.next_run_at_after(new.enabled, now_ms);
        Ok(Self {
            id,
            project_id,
            name: new.name,
            enabled: new.enabled,
            trigger: new.trigger,
            execution: new.execution,
            origin: new.origin,
            created_by_thread_id: new.created_by_thread_id,
            next_run_at,
            last_run_at: None,
            run_count: 0,
            consecutive_failures: 0,
            last_run_status: None,
            last_run_thread_id: None,
            last_error: None,
            created_at_ms: now_ms,
            updated_at_ms: now_ms,
        })
    }

    /// Applies a partial update.
    ///
    /// The merge is field-by-field, and a replacement trigger re-arms the
    /// schedule: a changed trigger recomputes `nextRunAt` when the automation
    /// is enabled and clears it when it is not, whereas a changed execution
    /// leaves the schedule alone — what a run does is independent of when it
    /// happens.
    pub fn update(&mut self, patch: &AutomationUpdate, now_ms: u64) -> Result<(), DomainError> {
        if patch.is_empty() {
            return Err(invalid("update", "at least one field is required"));
        }
        if patch.execution.is_some() && patch.agent.is_some() {
            return Err(invalid(
                "update",
                "execution and agent updates cannot be combined",
            ));
        }
        if let Some(name) = &patch.name {
            validate_name(name)?;
            self.name = name.clone();
        }
        if let Some(trigger) = &patch.trigger {
            trigger.validate(now_ms)?;
            self.next_run_at = trigger.next_run_at_after(self.enabled, now_ms);
            self.trigger = trigger.clone();
        }
        if let Some(execution) = &patch.execution {
            execution.validate()?;
            self.execution = execution.clone();
        }
        if let Some(agent) = &patch.agent {
            self.apply_agent_update(agent)?;
        }
        self.updated_at_ms = now_ms;
        Ok(())
    }

    /// Merges an `agent` patch into the stored agent execution.
    fn apply_agent_update(&mut self, patch: &AgentExecutionUpdate) -> Result<(), DomainError> {
        if patch.is_empty() {
            return Err(invalid(
                "update.agent",
                "at least one agent execution field is required",
            ));
        }
        let AutomationExecution::Agent(current) = &mut self.execution else {
            return Err(invalid(
                "update.agent",
                "agent execution options can only update agent automations",
            ));
        };
        if let Some(prompt) = &patch.prompt {
            validate_non_empty("update.agent.prompt", prompt)?;
            current.prompt = prompt.clone();
        }
        if let Some(provider_id) = &patch.provider_id {
            validate_non_empty("update.agent.providerId", provider_id)?;
            current.provider_id = provider_id.clone();
        }
        if let Some(model) = &patch.model {
            validate_non_empty("update.agent.model", model)?;
            current.model = model.clone();
        }
        if let Some(reasoning_level) = patch.reasoning_level {
            current.reasoning_level = reasoning_level;
        }
        if let Some(service_tier) = patch.service_tier {
            current.service_tier = service_tier;
        }
        if let Some(permission_mode) = patch.permission_mode {
            current.permission_mode = permission_mode;
        }
        if let Some(target) = &patch.target {
            match target {
                AgentExecutionTarget::TargetThread { thread_id } => {
                    current.target_thread_id = Some(thread_id.clone());
                }
                AgentExecutionTarget::Environment { environment } => {
                    environment.validate()?;
                    current.target_thread_id = None;
                    current.environment = environment.clone();
                }
            }
        }
        // The merge could have produced an execution that no longer validates
        // (an empty prompt from a legacy row, most obviously).
        AutomationExecution::Agent(current.clone()).validate()
    }

    /// A pause keeps everything but the schedule: nothing is reset, so a
    /// resume continues where the automation left off.
    pub fn pause(&mut self, now_ms: u64) {
        self.enabled = false;
        self.next_run_at = None;
        self.updated_at_ms = now_ms;
    }

    /// A resume re-arms the trigger and clears the failure state the pause was
    /// asked after.
    pub fn resume(&mut self, now_ms: u64) -> Result<(), DomainError> {
        self.trigger.validate(now_ms)?;
        self.enabled = true;
        self.next_run_at = self.trigger.next_run_at_after(true, now_ms);
        self.last_error = None;
        self.consecutive_failures = 0;
        self.updated_at_ms = now_ms;
        Ok(())
    }

    /// Whether the stored execution has no prompt, which makes every write
    /// operation on this automation a `409` until it is repaired.
    pub fn is_missing_agent_prompt(&self) -> bool {
        self.execution.is_missing_prompt()
    }

    /// The response projection.
    pub fn response(&self) -> AutomationResponse {
        AutomationResponse {
            id: self.id.to_string(),
            project_id: self.project_id.to_string(),
            name: self.name.clone(),
            enabled: self.enabled,
            trigger: self.trigger.clone(),
            execution: self.execution.clone(),
            origin: self.origin,
            created_by_thread_id: self.created_by_thread_id.as_ref().map(ToString::to_string),
            next_run_at: self.next_run_at,
            last_run_at: self.last_run_at,
            run_count: self.run_count,
            last_run_status: self.last_run_status,
            last_run_thread_id: self.last_run_thread_id.as_ref().map(ToString::to_string),
            last_error: self.last_error.clone(),
            created_at: self.created_at_ms,
            updated_at: self.updated_at_ms,
        }
    }
}

/// One attempt of an automation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutomationRun {
    /// Identity.
    pub id: AutomationRunId,
    /// The automation it belongs to.
    pub automation_id: AutomationId,
    /// What it executed.
    pub run_mode: AutomationRunMode,
    /// The thread it produced or was sent to, once there is one.
    pub thread_id: Option<ThreadId>,
    /// Where it is in its lifecycle.
    pub status: AutomationRunStatus,
    /// What asked for it.
    pub trigger: AutomationRunTrigger,
    /// Why it was skipped, when it was.
    pub skip_reason: Option<String>,
    /// Why it failed, when it did.
    pub error: Option<String>,
    /// What a script printed.
    pub output: Option<String>,
    /// What a script exited with.
    pub exit_code: Option<i32>,
    /// The client's deduplication key, when it sent one.
    ///
    /// Stored, and deliberately absent from the response: it is a transport
    /// detail of the request that created the row.
    pub idempotency_key: Option<String>,
    /// The instant the run was due at. For a manual run that is when it was
    /// asked for.
    pub scheduled_for: u64,
    /// When it started.
    pub started_at: u64,
    /// When it reached a terminal status.
    pub finished_at: Option<u64>,
}

impl AutomationRun {
    /// The row a manual `run` creates.
    ///
    /// It is `running` from the moment it exists: this stage records that a run
    /// was asked for, and the execution plane is what moves it to a terminal
    /// status.
    pub fn start_manual(
        id: AutomationRunId,
        automation_id: AutomationId,
        run_mode: AutomationRunMode,
        idempotency_key: Option<String>,
        now_ms: u64,
    ) -> Self {
        Self {
            id,
            automation_id,
            run_mode,
            thread_id: None,
            status: AutomationRunStatus::Running,
            trigger: AutomationRunTrigger::Manual,
            skip_reason: None,
            error: None,
            output: None,
            exit_code: None,
            idempotency_key,
            scheduled_for: now_ms,
            started_at: now_ms,
            finished_at: None,
        }
    }

    /// Whether the run is still in flight.
    pub fn is_running(&self) -> bool {
        self.status == AutomationRunStatus::Running
    }

    /// The response projection.
    pub fn response(&self) -> AutomationRunResponse {
        AutomationRunResponse {
            id: self.id.to_string(),
            automation_id: self.automation_id.to_string(),
            run_mode: self.run_mode,
            thread_id: self.thread_id.as_ref().map(ToString::to_string),
            status: self.status,
            trigger: self.trigger,
            skip_reason: self.skip_reason.clone(),
            error: self.error.clone(),
            output: self.output.clone(),
            exit_code: self.exit_code,
            scheduled_for: self.scheduled_for,
            started_at: self.started_at,
            finished_at: self.finished_at,
        }
    }
}

/// The mapping from a thread back to the automation and run that produced it.
///
/// A thread exists once and is produced by one run, so the thread id is the
/// key: a second mark for the same thread replaces the first. The execution
/// plane writes marks; the guard the server needs one for today is
/// [`Automation::create`]'s recursion check, which refuses an automation whose
/// `createdByThreadId` is itself automation-produced.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutomationThreadMark {
    /// The thread that was produced.
    pub thread_id: ThreadId,
    /// The automation that produced it.
    pub automation_id: AutomationId,
    /// The run that produced it.
    pub run_id: AutomationRunId,
    /// Wall-clock milliseconds when the mark was written.
    pub created_at_ms: u64,
}

/// The contract's automation response, `automationResponseSchema`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AutomationResponse {
    /// Automation id.
    pub id: String,
    /// Owning project id.
    pub project_id: String,
    /// Display name.
    pub name: String,
    /// Whether the trigger is live.
    pub enabled: bool,
    /// What starts a run.
    pub trigger: AutomationTrigger,
    /// What a run does.
    pub execution: AutomationExecution,
    /// Who asked for it.
    pub origin: AutomationOrigin,
    /// The creating thread, when an agent created it.
    pub created_by_thread_id: Option<String>,
    /// When the next run is due, when known.
    pub next_run_at: Option<u64>,
    /// When a run last started.
    pub last_run_at: Option<u64>,
    /// How many scheduled runs have started.
    pub run_count: u64,
    /// The status of the last finished run.
    pub last_run_status: Option<AutomationRunStatus>,
    /// The thread the last run produced or was sent to.
    pub last_run_thread_id: Option<String>,
    /// Why the last run failed.
    pub last_error: Option<String>,
    /// Creation time, epoch milliseconds.
    pub created_at: u64,
    /// Last mutation time, epoch milliseconds.
    pub updated_at: u64,
}

/// Why a stored automation cannot be used as it stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AutomationReadProblem {
    /// The stored row still carries an agent execution with no prompt.
    MissingAgentPrompt,
    /// The stored row does not describe an automation this build can read.
    InvalidStoredData,
}

/// A stored row that cannot be read, reduced to what identifies it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UnreadableAutomation {
    /// The id the row claims.
    pub id: String,
    /// The project the row claims.
    pub project_id: String,
    /// The name the row claims.
    pub name: String,
    /// Always `invalid-stored-data`.
    pub problem: AutomationReadProblem,
}

/// A row carried a legacy empty agent prompt, so it reads but cannot be used.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MissingPromptAutomation {
    /// The readable part of the row.
    #[serde(flatten)]
    pub automation: AutomationResponse,
    /// Always `missing-agent-prompt`.
    pub problem: AutomationReadProblem,
}

/// What a read operation returns for one stored automation.
///
/// The union exists because a stored row is data, not a value this process
/// just built: a row written by another build — or edited by hand — must be
/// reported so a client can offer to delete it, rather than being dropped
/// silently or failing the whole read.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AutomationReadResult {
    /// A usable automation.
    Automation(AutomationResponse),
    /// A row that parses but cannot be used until its prompt is set.
    MissingAgentPrompt(MissingPromptAutomation),
    /// A row that does not parse.
    InvalidStoredData(UnreadableAutomation),
}

/// The contract's run projection, `automationRunResponseSchema`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AutomationRunResponse {
    /// Run id.
    pub id: String,
    /// Owning automation id.
    pub automation_id: String,
    /// What the run executed.
    pub run_mode: AutomationRunMode,
    /// The thread it produced or was sent to.
    pub thread_id: Option<String>,
    /// Where it is in its lifecycle.
    pub status: AutomationRunStatus,
    /// What asked for it.
    pub trigger: AutomationRunTrigger,
    /// Why it was skipped.
    pub skip_reason: Option<String>,
    /// Why it failed.
    pub error: Option<String>,
    /// What a script printed.
    pub output: Option<String>,
    /// What a script exited with.
    pub exit_code: Option<i32>,
    /// When it was due.
    pub scheduled_for: u64,
    /// When it started.
    pub started_at: u64,
    /// When it finished.
    pub finished_at: Option<u64>,
}

/// One automation and the project it belongs to, for the overview listing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AutomationOverviewEntry {
    /// The automation, or why it cannot be read.
    pub automation: AutomationReadResult,
    /// The owning project, by id and name.
    pub project: AutomationProjectSummary,
}

/// The project a listed automation belongs to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AutomationProjectSummary {
    /// Project id.
    pub id: String,
    /// Project name.
    pub name: String,
}

/// Rejects a name the contract's bounds do not allow.
pub fn validate_name(name: &str) -> Result<(), DomainError> {
    if name.is_empty() {
        return Err(invalid("name", "must not be empty"));
    }
    if name.chars().count() > AUTOMATION_NAME_MAX_LENGTH {
        return Err(invalid(
            "name",
            format!("must be at most {AUTOMATION_NAME_MAX_LENGTH} characters"),
        ));
    }
    Ok(())
}

/// Rejects an idempotency key the contract's bounds do not allow.
pub fn validate_idempotency_key(key: &str) -> Result<(), DomainError> {
    if key.is_empty() {
        return Err(invalid("idempotencyKey", "must not be empty"));
    }
    if key.chars().count() > AUTOMATION_IDEMPOTENCY_KEY_MAX_LENGTH {
        return Err(invalid(
            "idempotencyKey",
            format!("must be at most {AUTOMATION_IDEMPOTENCY_KEY_MAX_LENGTH} characters"),
        ));
    }
    Ok(())
}

/// Rejects a run-list page size outside the contract's bounds.
pub fn validate_runs_limit(limit: u32) -> Result<(), DomainError> {
    if limit == 0 || limit > AUTOMATION_RUNS_LIMIT_MAX {
        return Err(invalid(
            "limit",
            format!("must be between 1 and {AUTOMATION_RUNS_LIMIT_MAX}"),
        ));
    }
    Ok(())
}

/// Rejects a cron expression this server cannot commit to evaluating.
///
/// The grammar is the five-field form — `minute hour day-of-month month
/// day-of-week` — with `*`, `?` (day fields), lists, ranges, steps and the
/// three-letter month and day names. The whole expression is validated here,
/// including which numbers each field admits, because a stored expression the
/// scheduler later refuses would be a schedule that silently never fires.
pub fn validate_cron(expression: &str) -> Result<(), DomainError> {
    if expression.is_empty() {
        return Err(invalid("trigger.cron", "must not be empty"));
    }
    if expression.chars().count() > SCHEDULE_CRON_MAX_LENGTH {
        return Err(invalid(
            "trigger.cron",
            format!("must be at most {SCHEDULE_CRON_MAX_LENGTH} characters"),
        ));
    }
    let fields: Vec<&str> = expression.split_whitespace().collect();
    if fields.len() != 5 {
        return Err(invalid(
            "trigger.cron",
            "must have exactly 5 fields (minute hour day-of-month month day-of-week)",
        ));
    }
    for (index, field) in fields.iter().enumerate() {
        validate_cron_field(field, index).map_err(|reason| invalid("trigger.cron", reason))?;
    }
    Ok(())
}

/// Validates one cron field against its own bounds.
fn validate_cron_field(field: &str, index: usize) -> Result<(), String> {
    let bounds = CronFieldBounds::at(index);
    for term in field.split(',') {
        if term.is_empty() {
            return Err(format!("empty list entry in {:?}", field));
        }
        let (range, step) = match term.split_once('/') {
            Some((range, step)) => (range, Some(step)),
            None => (term, None),
        };
        if let Some(step) = step {
            let step: u32 = step
                .parse()
                .map_err(|_| format!("step {step:?} is not a number"))?;
            if step == 0 {
                return Err(format!("step in {term:?} must be at least 1"));
            }
        }
        match range {
            "*" => {}
            "?" if bounds.question_mark => {}
            value => {
                let (start, end) = match value.split_once('-') {
                    Some((start, end)) => (start, end),
                    None => (value, value),
                };
                let start = bounds
                    .parse(start)
                    .ok_or_else(|| bounds.out_of_range(value))?;
                let end = bounds
                    .parse(end)
                    .ok_or_else(|| bounds.out_of_range(value))?;
                if start > end {
                    return Err(format!("range {value:?} runs backwards"));
                }
            }
        }
    }
    Ok(())
}

/// One cron field's admissible values.
struct CronFieldBounds {
    /// Lowest value.
    min: u32,
    /// Highest value.
    max: u32,
    /// Three-letter aliases, when the field has them.
    names: &'static [&'static str],
    /// Whether `?` is accepted as `*`, which only the day fields allow.
    question_mark: bool,
    /// The field's name, for error messages.
    label: &'static str,
}

impl CronFieldBounds {
    fn at(index: usize) -> Self {
        match index {
            0 => Self {
                min: 0,
                max: 59,
                names: &[],
                question_mark: false,
                label: "minute",
            },
            1 => Self {
                min: 0,
                max: 23,
                names: &[],
                question_mark: false,
                label: "hour",
            },
            2 => Self {
                min: 1,
                max: 31,
                names: &[],
                question_mark: true,
                label: "day-of-month",
            },
            3 => Self {
                min: 1,
                max: 12,
                names: &[
                    "JAN", "FEB", "MAR", "APR", "MAY", "JUN", "JUL", "AUG", "SEP", "OCT", "NOV",
                    "DEC",
                ],
                question_mark: false,
                label: "month",
            },
            _ => Self {
                min: 0,
                max: 7,
                names: &["SUN", "MON", "TUE", "WED", "THU", "FRI", "SAT"],
                question_mark: true,
                label: "day-of-week",
            },
        }
    }

    fn parse(&self, value: &str) -> Option<u32> {
        if value.is_empty() {
            return None;
        }
        if !self.names.is_empty() {
            let upper = value.to_ascii_uppercase();
            if let Some(position) = self.names.iter().position(|name| *name == upper) {
                // Months are 1-based, weekdays 0-based; both alias tables start
                // at their first value.
                return Some(if self.min == 1 {
                    position as u32 + 1
                } else {
                    position as u32
                });
            }
        }
        let number: u32 = value.parse().ok()?;
        (number >= self.min && number <= self.max).then_some(number)
    }

    fn out_of_range(&self, value: &str) -> String {
        format!(
            "{value:?} is not a valid {} value ({}–{})",
            self.label, self.min, self.max
        )
    }
}

/// Rejects a timezone name that is not shaped like an IANA zone.
///
/// Only the name's shape is checked. Resolving it to an offset — which is what
/// a scheduler needs — requires a timezone database, and adding one before
/// there is a scheduler would turn a syntax check into a claim this build
/// cannot keep. An unknown name that is *shaped* correctly is therefore
/// accepted here and will be rejected by the scheduler phase, which is where
/// the database lives.
pub fn validate_timezone(timezone: &str) -> Result<(), DomainError> {
    if timezone.is_empty() {
        return Err(invalid("trigger.timezone", "must not be empty"));
    }
    if timezone.chars().count() > SCHEDULE_TIMEZONE_MAX_LENGTH {
        return Err(invalid(
            "trigger.timezone",
            format!("must be at most {SCHEDULE_TIMEZONE_MAX_LENGTH} characters"),
        ));
    }
    let components: Vec<&str> = timezone.split('/').collect();
    if components.len() > 3 {
        return Err(invalid("trigger.timezone", "has too many path segments"));
    }
    for component in components {
        if component.is_empty() {
            return Err(invalid("trigger.timezone", "has an empty path segment"));
        }
        let shaped = component.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '+')
        });
        if !shaped {
            return Err(invalid(
                "trigger.timezone",
                format!("{component:?} is not a valid timezone component"),
            ));
        }
    }
    Ok(())
}

fn validate_non_empty(field: &'static str, value: &str) -> Result<(), DomainError> {
    if value.is_empty() {
        return Err(invalid(field, "must not be empty"));
    }
    Ok(())
}

/// A field rejection the HTTP surface reports as `400 invalid_request`.
pub fn invalid(field: &'static str, reason: impl Into<String>) -> DomainError {
    DomainError::InvalidField {
        field,
        reason: reason.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> u64 {
        1_800_000_000_000
    }

    fn project() -> ProjectId {
        ProjectId::mint()
    }

    fn agent_execution() -> AutomationExecution {
        AutomationExecution::Agent(AgentExecution {
            prompt: "summarise the repository".into(),
            provider_id: "pi".into(),
            model: "pi/default".into(),
            reasoning_level: ReasoningLevel::Medium,
            service_tier: None,
            permission_mode: PermissionMode::Auto,
            environment: AgentEnvironment::ProjectDefault,
            target_thread_id: None,
        })
    }

    fn schedule() -> AutomationTrigger {
        AutomationTrigger::Schedule {
            cron: "0 9 * * 1-5".into(),
            timezone: "Europe/Paris".into(),
        }
    }

    fn new_automation(trigger: AutomationTrigger) -> NewAutomation {
        NewAutomation {
            name: "Nightly".into(),
            enabled: true,
            trigger,
            execution: agent_execution(),
            origin: AutomationOrigin::Human,
            created_by_thread_id: None,
        }
    }

    fn automation(trigger: AutomationTrigger) -> Automation {
        Automation::create(
            AutomationId::mint(),
            project(),
            new_automation(trigger),
            now(),
        )
        .expect("the fixture is valid")
    }

    #[test]
    fn a_schedule_does_not_claim_a_next_run_this_build_cannot_compute() {
        let automation = automation(schedule());
        assert_eq!(automation.next_run_at, None);
        assert_eq!(automation.trigger.kind(), "schedule");
    }

    #[test]
    fn a_once_trigger_arms_its_own_instant_and_refuses_the_past() {
        let armed = automation(AutomationTrigger::Once {
            run_at: now() + 60_000,
        });
        assert_eq!(armed.next_run_at, Some(now() + 60_000));

        assert!(matches!(
            Automation::create(
                AutomationId::mint(),
                project(),
                new_automation(AutomationTrigger::Once { run_at: now() }),
                now(),
            ),
            Err(DomainError::InvalidField {
                field: "trigger.runAt",
                ..
            })
        ));
    }

    #[test]
    fn a_disabled_once_trigger_is_not_armed() {
        let mut new = new_automation(AutomationTrigger::Once {
            run_at: now() + 60_000,
        });
        new.enabled = false;
        let automation =
            Automation::create(AutomationId::mint(), project(), new, now()).expect("valid");
        assert!(!automation.enabled);
        assert_eq!(automation.next_run_at, None);
    }

    #[test]
    fn cron_expressions_are_validated_field_by_field() {
        for expression in [
            "* * * * *",
            "*/15 0-23/2 1,15 * MON-FRI",
            "0 0 1 JAN *",
            "30 6 ? * 7",
            "5/15 * * * *",
        ] {
            assert!(
                validate_cron(expression).is_ok(),
                "{expression} should be valid"
            );
        }
        for expression in [
            "",
            "* * * *",
            "* * * * * *",
            "60 * * * *",
            "* 24 * * *",
            "* * 0 * *",
            "* * * 13 *",
            "* * * * 8",
            "*/0 * * * *",
            "5-1 * * * *",
            "1,,2 * * * *",
            "* * * FOO *",
            "* * L * *",
        ] {
            assert!(
                validate_cron(expression).is_err(),
                "{expression} should be rejected"
            );
        }
    }

    #[test]
    fn a_cron_expression_at_the_length_limit_is_still_accepted() {
        // One long, valid list field padded so the whole expression lands on
        // the exact limit; one entry more is refused as too long.
        let minute = format!("{}0", "0,".repeat(43));
        let expression = format!("{minute} 0-23/2 * * *");
        assert_eq!(expression.chars().count(), SCHEDULE_CRON_MAX_LENGTH);
        assert!(validate_cron(&expression).is_ok());

        let too_long = format!("0,{expression}");
        assert!(matches!(
            validate_cron(&too_long),
            Err(DomainError::InvalidField { field: "trigger.cron", reason })
                if reason.contains(&SCHEDULE_CRON_MAX_LENGTH.to_string())
        ));
    }

    #[test]
    fn timezones_are_checked_for_shape_not_for_existence() {
        for timezone in [
            "UTC",
            "Europe/Paris",
            "America/Argentina/Buenos_Aires",
            "Etc/GMT+5",
        ] {
            assert!(
                validate_timezone(timezone).is_ok(),
                "{timezone} should be valid"
            );
        }
        for timezone in [
            "",
            "Europe//Paris",
            "Europe/Paris/Now/Berlin",
            "/Paris",
            "Europe Paris",
            "Europe/Paris;",
        ] {
            assert!(
                validate_timezone(timezone).is_err(),
                "{timezone} should be rejected"
            );
        }
    }

    #[test]
    fn a_script_automation_needs_exactly_one_source() {
        let script = |script: Option<&str>, script_file: Option<&str>| ScriptExecution {
            script: script.map(str::to_owned),
            script_file: script_file.map(str::to_owned),
            interpreter: None,
            timeout_ms: AUTOMATION_SCRIPT_TIMEOUT_DEFAULT_MS,
            env: None,
        };
        assert!(script(Some("echo hi"), None).validate().is_ok());
        assert!(script(None, Some("/srv/job.sh")).validate().is_ok());
        assert!(script(None, None).validate().is_err());
        assert!(script(Some("echo hi"), Some("/srv/job.sh"))
            .validate()
            .is_err());
        assert!(script(Some(""), None).validate().is_err());
        assert!(script(None, Some("")).validate().is_err());
        assert!(
            script(Some(&"x".repeat(AUTOMATION_SCRIPT_MAX_LENGTH + 1)), None)
                .validate()
                .is_err()
        );
        assert!(script(
            None,
            Some(&"x".repeat(AUTOMATION_SCRIPT_FILE_MAX_LENGTH + 1))
        )
        .validate()
        .is_err());
    }

    #[test]
    fn a_script_timeout_must_be_within_the_contract_bounds() {
        let mut execution = ScriptExecution {
            script: Some("echo hi".into()),
            script_file: None,
            interpreter: Some(ScriptInterpreter::Bash),
            timeout_ms: 0,
            env: None,
        };
        assert!(execution.validate().is_err());
        execution.timeout_ms = AUTOMATION_SCRIPT_TIMEOUT_MAX_MS;
        assert!(execution.validate().is_ok());
        execution.timeout_ms = AUTOMATION_SCRIPT_TIMEOUT_MAX_MS + 1;
        assert!(execution.validate().is_err());
    }

    #[test]
    fn a_host_environment_needs_a_host_unless_the_workspace_is_personal() {
        let host_without_id = AgentEnvironment::Host {
            host_id: None,
            workspace: WorkspaceKind::ManagedWorktree {
                base_branch: ManagedBaseBranch::Default,
            },
        };
        assert!(host_without_id.validate().is_err());

        let personal = AgentEnvironment::Host {
            host_id: None,
            workspace: WorkspaceKind::Personal,
        };
        assert!(personal.validate().is_ok());
    }

    #[test]
    fn names_and_idempotency_keys_are_bounded() {
        assert!(validate_name("nightly").is_ok());
        assert!(validate_name("").is_err());
        assert!(validate_name(&"x".repeat(AUTOMATION_NAME_MAX_LENGTH)).is_ok());
        assert!(validate_name(&"x".repeat(AUTOMATION_NAME_MAX_LENGTH + 1)).is_err());

        assert!(validate_idempotency_key("key-1").is_ok());
        assert!(validate_idempotency_key("").is_err());
        assert!(
            validate_idempotency_key(&"k".repeat(AUTOMATION_IDEMPOTENCY_KEY_MAX_LENGTH + 1))
                .is_err()
        );
    }

    #[test]
    fn an_update_merges_field_by_field() {
        let mut automation = automation(schedule());
        let patch = AutomationUpdate {
            name: Some("Morning".into()),
            ..Default::default()
        };
        automation.update(&patch, now() + 1).expect("valid");
        assert_eq!(automation.name, "Morning");
        assert_eq!(automation.trigger, schedule());
        assert_eq!(automation.updated_at_ms, now() + 1);
    }

    #[test]
    fn an_update_with_nothing_to_change_or_two_executions_is_rejected() {
        let mut automation = automation(schedule());
        assert!(matches!(
            automation.update(&AutomationUpdate::default(), now()),
            Err(DomainError::InvalidField {
                field: "update",
                ..
            })
        ));

        let both = AutomationUpdate {
            execution: Some(agent_execution()),
            agent: Some(AgentExecutionUpdate {
                model: Some("pi/other".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(matches!(
            automation.update(&both, now()),
            Err(DomainError::InvalidField {
                field: "update",
                ..
            })
        ));
    }

    #[test]
    fn replacing_the_trigger_rearms_the_schedule_only_while_enabled() {
        let mut armed = automation(schedule());
        armed
            .update(
                &AutomationUpdate {
                    trigger: Some(AutomationTrigger::Once {
                        run_at: now() + 1_000,
                    }),
                    ..Default::default()
                },
                now(),
            )
            .expect("valid");
        assert_eq!(armed.next_run_at, Some(now() + 1_000));

        let mut paused = automation(schedule());
        paused.pause(now());
        paused
            .update(
                &AutomationUpdate {
                    trigger: Some(AutomationTrigger::Once {
                        run_at: now() + 1_000,
                    }),
                    ..Default::default()
                },
                now(),
            )
            .expect("valid");
        assert_eq!(paused.next_run_at, None);
    }

    #[test]
    fn an_agent_patch_merges_and_a_null_service_tier_clears_it() {
        let mut automation = automation(schedule());
        if let AutomationExecution::Agent(agent) = &mut automation.execution {
            agent.service_tier = Some(ServiceTier::Fast);
        }
        automation
            .update(
                &AutomationUpdate {
                    agent: Some(AgentExecutionUpdate {
                        model: Some("pi/faster".into()),
                        service_tier: Some(None),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                now(),
            )
            .expect("valid");
        let AutomationExecution::Agent(agent) = &automation.execution else {
            panic!("still an agent execution");
        };
        assert_eq!(agent.model, "pi/faster");
        assert_eq!(agent.service_tier, None);
        assert_eq!(agent.prompt, "summarise the repository");
    }

    #[test]
    fn an_agent_patch_retargets_a_thread_and_an_environment_clears_it() {
        let mut automation = automation(schedule());
        let thread_id = ThreadId::mint();
        automation
            .update(
                &AutomationUpdate {
                    agent: Some(AgentExecutionUpdate {
                        target: Some(AgentExecutionTarget::TargetThread {
                            thread_id: thread_id.clone(),
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                now(),
            )
            .expect("valid");
        assert_eq!(automation.execution.target_thread_id(), Some(&thread_id));

        automation
            .update(
                &AutomationUpdate {
                    agent: Some(AgentExecutionUpdate {
                        target: Some(AgentExecutionTarget::Environment {
                            environment: AgentEnvironment::ProjectDefault,
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                now(),
            )
            .expect("valid");
        assert_eq!(automation.execution.target_thread_id(), None);
    }

    #[test]
    fn an_agent_patch_on_a_script_automation_is_rejected() {
        let mut automation = automation(schedule());
        automation.execution = AutomationExecution::Script(ScriptExecution {
            script: Some("echo hi".into()),
            script_file: None,
            interpreter: None,
            timeout_ms: AUTOMATION_SCRIPT_TIMEOUT_DEFAULT_MS,
            env: None,
        });
        assert!(matches!(
            automation.update(
                &AutomationUpdate {
                    agent: Some(AgentExecutionUpdate {
                        model: Some("pi/other".into()),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                now(),
            ),
            Err(DomainError::InvalidField {
                field: "update.agent",
                ..
            })
        ));
    }

    #[test]
    fn a_legacy_empty_prompt_reads_but_needs_a_prompt_to_be_repaired() {
        let mut automation = automation(schedule());
        let AutomationExecution::Agent(agent) = &mut automation.execution else {
            panic!("agent execution");
        };
        agent.prompt = String::new();
        assert!(automation.is_missing_agent_prompt());

        // The domain keeps a legacy row readable: repairing it with a prompt
        // works, and so does any change that leaves the empty prompt alone.
        // Refusing the latter is the write rule the HTTP surface applies on
        // top (`service.ts` in the reference implementation does the same).
        automation
            .update(
                &AutomationUpdate {
                    name: Some("Still broken".into()),
                    ..Default::default()
                },
                now(),
            )
            .expect("the domain does not own the write rule");
        assert!(automation.is_missing_agent_prompt());

        automation
            .update(
                &AutomationUpdate {
                    agent: Some(AgentExecutionUpdate {
                        prompt: Some("now it has one".into()),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                now(),
            )
            .expect("a prompt repairs it");
        assert!(!automation.is_missing_agent_prompt());
    }

    #[test]
    fn pause_clears_the_schedule_and_resume_rearms_it() {
        let mut automation = automation(AutomationTrigger::Once {
            run_at: now() + 60_000,
        });
        automation.last_error = Some("boom".into());
        automation.consecutive_failures = 2;
        automation.pause(now() + 1);
        assert!(!automation.enabled);
        assert_eq!(automation.next_run_at, None);
        assert_eq!(automation.last_error.as_deref(), Some("boom"));
        assert_eq!(automation.consecutive_failures, 2);

        automation.resume(now() + 2).expect("valid");
        assert!(automation.enabled);
        assert_eq!(automation.next_run_at, Some(now() + 60_000));
        assert_eq!(automation.last_error, None);
        assert_eq!(automation.consecutive_failures, 0);
        assert_eq!(automation.updated_at_ms, now() + 2);
    }

    #[test]
    fn resuming_an_expired_one_shot_is_rejected() {
        let mut automation = automation(AutomationTrigger::Once {
            run_at: now() + 60_000,
        });
        automation.pause(now());
        assert!(matches!(
            automation.resume(now() + 60_000),
            Err(DomainError::InvalidField {
                field: "trigger.runAt",
                ..
            })
        ));
    }

    #[test]
    fn a_manual_run_is_running_and_carries_no_result() {
        let run = AutomationRun::start_manual(
            AutomationRunId::mint(),
            AutomationId::mint(),
            AutomationRunMode::Agent,
            Some("key".into()),
            now(),
        );
        assert_eq!(run.status, AutomationRunStatus::Running);
        assert_eq!(run.trigger, AutomationRunTrigger::Manual);
        assert_eq!(run.scheduled_for, now());
        assert_eq!(run.started_at, now());
        assert_eq!(run.finished_at, None);
        assert!(run.is_running());
        let value = serde_json::to_value(run.response()).expect("serializable");
        assert!(value.get("idempotencyKey").is_none());
        assert_eq!(value["status"], "running");
        assert_eq!(value["trigger"], "manual");
    }

    #[test]
    fn the_response_projection_matches_the_contract_field_names() {
        let automation = automation(schedule());
        let value = serde_json::to_value(automation.response()).expect("serializable");
        assert_eq!(value["id"], automation.id.to_string());
        assert_eq!(value["projectId"], automation.project_id.to_string());
        assert_eq!(value["createdAt"], now());
        assert_eq!(value["updatedAt"], now());
        assert_eq!(value["nextRunAt"], serde_json::Value::Null);
        assert_eq!(value["trigger"]["triggerType"], "schedule");
        assert_eq!(value["trigger"]["cron"], "0 9 * * 1-5");
        assert_eq!(value["execution"]["mode"], "agent");
        assert_eq!(value["execution"]["reasoningLevel"], "medium");
        assert_eq!(value["execution"]["environment"]["type"], "project-default");
        assert!(value.get("consecutiveFailures").is_none());
    }

    #[test]
    fn an_unmanaged_workspace_always_writes_its_path_key() {
        // The contract spells `path` as `z.string().min(1).nullable()` inside a
        // `.strict()` object, so every response for an `unmanaged` workspace
        // must carry the key even when there is no path. Omitting it made the
        // response fail the contract's own validation.
        for (label, workspace) in [
            (
                "absent",
                WorkspaceKind::Unmanaged {
                    path: None,
                    branch: None,
                },
            ),
            (
                "set",
                WorkspaceKind::Unmanaged {
                    path: Some("/srv/loom".into()),
                    branch: None,
                },
            ),
        ] {
            let value = serde_json::to_value(&workspace).expect("serializable");
            assert!(
                value.as_object().expect("an object").contains_key("path"),
                "the {label} path still writes the required `path` key: {value}"
            );
        }
        let absent = serde_json::to_value(WorkspaceKind::Unmanaged {
            path: None,
            branch: None,
        })
        .expect("serializable");
        assert_eq!(absent["path"], serde_json::Value::Null);
    }

    #[test]
    fn a_host_environment_keeps_the_contract_field_set_in_its_response() {
        // The execution projection carries a host environment end to end. The
        // unmanaged workspace is the one that used to lose its `path` key, so
        // the projection is asserted through the full response, not just the
        // workspace on its own.
        let mut automation = automation(schedule());
        let AutomationExecution::Agent(agent) = &mut automation.execution else {
            panic!("agent execution");
        };
        agent.environment = AgentEnvironment::Host {
            host_id: Some(HostId::mint()),
            workspace: WorkspaceKind::Unmanaged {
                path: None,
                branch: None,
            },
        };
        let value = serde_json::to_value(automation.response()).expect("serializable");
        let workspace = &value["execution"]["environment"]["workspace"];
        assert_eq!(workspace["type"], "unmanaged");
        assert_eq!(workspace["path"], serde_json::Value::Null);
        assert!(workspace
            .as_object()
            .expect("an object")
            .contains_key("path"));
    }
}
