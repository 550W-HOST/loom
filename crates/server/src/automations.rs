//! Automations: durable storage and the typed HTTP surface.
//!
//! An automation is a project-owned trigger plus the execution it starts (see
//! [`loom_domain::automation`]). This module owns three things:
//!
//! 1. **The stored row** ([`StoredAutomation`]): what a snapshot holds. Rows
//!    are stored as data, not as a decoded value, for the same reason the
//!    reference implementation stores SQLite columns: a row written by another
//!    build — or edited by hand — must be *reportable* rather than fatal. Every
//!    row therefore deserializes with defaults, and decoding is a separate,
//!    fallible step that produces either an automation or a read problem.
//! 2. **The registry** ([`AutomationsRegistry`]): the in-memory rows behind one
//!    mutex, with the command operations. It rides the existing domain
//!    snapshot (`DomainSnapshot::automations`), so there is no second database:
//!    a mutation writes the snapshot the same way a settings write does, and a
//!    restart restores it.
//! 3. **The HTTP handlers**: ten routes, scoped by project and automation id.
//!
//! # Why these are not relay events
//!
//! The domain registry is a projection of the relay log, replayed on top of
//! the snapshot. Automations are not: nothing in the log describes them, so
//! keeping them in that registry would mean a restored snapshot could be
//! *replayed over* by events that never mention them. They follow
//! `settings.rs` instead — a versioned payload inside the same snapshot,
//! written synchronously after each mutation, with its own version so additive
//! changes need no outer-format bump.
//!
//! # What this stage does not do
//!
//! No scheduler, no execution, no realtime invalidation and no UI. `run`
//! records that a run was asked for and returns it; the row stays `running`
//! until the execution stage reports a terminal status for it. A cron
//! schedule's `nextRunAt` stays `null` for the same reason: computing it needs
//! the timezone-aware scheduler that stage brings, and a number this server
//! would not honour is worse than an absent one.

#![allow(clippy::result_large_err)]

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use loom_domain::automation::{
    validate_idempotency_key, validate_runs_limit, Automation, AutomationExecution,
    AutomationOrigin, AutomationReadProblem, AutomationReadResult, AutomationRun,
    AutomationRunMode, AutomationRunStatus, AutomationRunTrigger, AutomationThreadMark,
    AutomationTrigger, AutomationUpdate, MissingPromptAutomation, NewAutomation,
    UnreadableAutomation,
};
use loom_domain::{AutomationId, AutomationRunId, ProjectId, ThreadId};

use crate::state::AppState;

/// The current automation payload version.
///
/// Version 0 is a payload written before the field existed: it loads with the
/// same additive defaults (`#[serde(default)]` on every row field) a version-1
/// payload gets for a field added later. A *newer* version is not interpreted
/// at all — see [`AutomationsRegistry::restore`].
pub const AUTOMATIONS_VERSION: u32 = 1;

/* ------------------------------------------------------------------ */
/* Stored rows                                                         */
/* ------------------------------------------------------------------ */

/// One automation as it is stored.
///
/// Every field defaults, so any JSON object in a snapshot deserializes: a row
/// missing an additive field keeps working, and a row this build cannot read
/// is reported as `invalid-stored-data` instead of taking the payload — or the
/// whole snapshot — with it.
///
/// `trigger` and `execution` are kept as raw JSON, exactly as the reference
/// implementation keeps them as separate columns, because they are the two
/// halves a legacy or damaged row typically breaks in. `triggerType` and
/// `runMode` are the stored discriminators; they must agree with the JSON they
/// describe, and a row where they do not is invalid stored data rather than a
/// value this build guesses at.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredAutomation {
    /// The row's id.
    #[serde(default)]
    pub id: String,
    /// The owning project.
    #[serde(default)]
    pub project_id: String,
    /// Display name.
    #[serde(default)]
    pub name: String,
    /// Whether the trigger is live.
    #[serde(default)]
    pub enabled: bool,
    /// The stored discriminator for `trigger`.
    #[serde(default)]
    pub trigger_type: Option<String>,
    /// The trigger, as stored.
    #[serde(default)]
    pub trigger: Value,
    /// The stored discriminator for `execution`.
    #[serde(default)]
    pub run_mode: Option<String>,
    /// The execution, as stored.
    #[serde(default)]
    pub execution: Value,
    /// Who created it.
    #[serde(default)]
    pub origin: Option<String>,
    /// The creating thread.
    #[serde(default)]
    pub created_by_thread_id: Option<String>,
    /// When the next run is due.
    #[serde(default)]
    pub next_run_at: Option<u64>,
    /// When a run last started.
    #[serde(default)]
    pub last_run_at: Option<u64>,
    /// How many scheduled runs started.
    #[serde(default)]
    pub run_count: u64,
    /// How many runs failed in a row.
    #[serde(default)]
    pub consecutive_failures: u32,
    /// The last finished run's status.
    #[serde(default)]
    pub last_run_status: Option<String>,
    /// The thread the last run produced or was sent to.
    #[serde(default)]
    pub last_run_thread_id: Option<String>,
    /// Why the last run failed.
    #[serde(default)]
    pub last_error: Option<String>,
    /// Creation time, epoch milliseconds.
    #[serde(default)]
    pub created_at: u64,
    /// Last mutation time, epoch milliseconds.
    #[serde(default)]
    pub updated_at: u64,
}

/// One automation run as it is stored.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredAutomationRun {
    /// Run id.
    #[serde(default)]
    pub id: String,
    /// Owning automation.
    #[serde(default)]
    pub automation_id: String,
    /// What the run executes.
    #[serde(default)]
    pub run_mode: String,
    /// The thread it produced or was sent to.
    #[serde(default)]
    pub thread_id: Option<String>,
    /// Where it is in its lifecycle.
    #[serde(default)]
    pub status: String,
    /// What asked for it.
    #[serde(default)]
    pub trigger: String,
    /// Why it was skipped.
    #[serde(default)]
    pub skip_reason: Option<String>,
    /// Why it failed.
    #[serde(default)]
    pub error: Option<String>,
    /// What a script printed.
    #[serde(default)]
    pub output: Option<String>,
    /// What a script exited with.
    #[serde(default)]
    pub exit_code: Option<i32>,
    /// The client's deduplication key.
    #[serde(default)]
    pub idempotency_key: Option<String>,
    /// When the run was due.
    #[serde(default)]
    pub scheduled_for: u64,
    /// When it started.
    #[serde(default)]
    pub started_at: u64,
    /// When it finished.
    #[serde(default)]
    pub finished_at: Option<u64>,
}

/// The thread-to-automation mapping as it is stored.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredAutomationThreadMark {
    /// The produced thread.
    #[serde(default)]
    pub thread_id: String,
    /// The automation that produced it.
    #[serde(default)]
    pub automation_id: String,
    /// The run that produced it.
    #[serde(default)]
    pub run_id: String,
    /// When the mark was written.
    #[serde(default)]
    pub created_at: u64,
}

/* ------------------------------------------------------------------ */
/* Decoding                                                            */
/* ------------------------------------------------------------------ */

/// Decodes a stored row, or reports why it cannot be read.
///
/// The checks are the reference implementation's: the stored discriminators
/// must agree with the JSON they describe, and the stored target thread must
/// agree with the execution's own target. A disagreement is `invalid-stored-data`
/// — the row is reported, never repaired by guesswork.
fn decode(row: &StoredAutomation) -> Result<Automation, ()> {
    let id: AutomationId = row.id.parse().map_err(|_| ())?;
    let project_id: ProjectId = row.project_id.parse().map_err(|_| ())?;
    let trigger: AutomationTrigger = serde_json::from_value(row.trigger.clone()).map_err(|_| ())?;
    let execution: AutomationExecution =
        serde_json::from_value(row.execution.clone()).map_err(|_| ())?;
    // A row must still satisfy the contract's rules to be readable; the only
    // exception is the legacy empty agent prompt, which stays readable and is
    // reported as `missing-agent-prompt` instead.
    execution.validate_stored().map_err(|_| ())?;
    let origin = match row.origin.as_deref() {
        Some("human") => AutomationOrigin::Human,
        Some("app") => AutomationOrigin::App,
        Some("agent") => AutomationOrigin::Agent,
        _ => return Err(()),
    };
    if row
        .trigger_type
        .as_deref()
        .is_some_and(|kind| kind != trigger.kind())
    {
        return Err(());
    }
    if row
        .run_mode
        .as_deref()
        .is_some_and(|mode| mode != run_mode_token(execution.run_mode()))
    {
        return Err(());
    }
    let last_run_status = match row.last_run_status.as_deref() {
        None | Some("") => None,
        Some("running") => Some(AutomationRunStatus::Running),
        Some("succeeded") => Some(AutomationRunStatus::Succeeded),
        Some("failed") => Some(AutomationRunStatus::Failed),
        Some("skipped") => Some(AutomationRunStatus::Skipped),
        _ => return Err(()),
    };
    Ok(Automation {
        id,
        project_id,
        name: row.name.clone(),
        enabled: row.enabled,
        trigger,
        execution,
        origin,
        created_by_thread_id: parse_optional_thread(&row.created_by_thread_id)?,
        next_run_at: row.next_run_at,
        last_run_at: row.last_run_at,
        run_count: row.run_count,
        consecutive_failures: row.consecutive_failures,
        last_run_status,
        last_run_thread_id: parse_optional_thread(&row.last_run_thread_id)?,
        last_error: row.last_error.clone(),
        created_at_ms: row.created_at,
        updated_at_ms: row.updated_at,
    })
}

fn parse_optional_thread(value: &Option<String>) -> Result<Option<ThreadId>, ()> {
    match value {
        None => Ok(None),
        Some(value) => value.parse().map(Some).map_err(|_| ()),
    }
}

/// The stored token for a run mode.
fn run_mode_token(mode: AutomationRunMode) -> &'static str {
    match mode {
        AutomationRunMode::Agent => "agent",
        AutomationRunMode::Script => "script",
    }
}

/// The stored token for a run status.
fn run_status_token(status: AutomationRunStatus) -> &'static str {
    match status {
        AutomationRunStatus::Running => "running",
        AutomationRunStatus::Succeeded => "succeeded",
        AutomationRunStatus::Failed => "failed",
        AutomationRunStatus::Skipped => "skipped",
    }
}

/// The stored token for a run trigger.
fn run_trigger_token(trigger: AutomationRunTrigger) -> &'static str {
    match trigger {
        AutomationRunTrigger::Schedule => "schedule",
        AutomationRunTrigger::Manual => "manual",
    }
}

/// Encodes a decoded automation back into its stored form.
fn encode(automation: &Automation) -> StoredAutomation {
    StoredAutomation {
        id: automation.id.to_string(),
        project_id: automation.project_id.to_string(),
        name: automation.name.clone(),
        enabled: automation.enabled,
        trigger_type: Some(automation.trigger.kind().to_owned()),
        trigger: serde_json::to_value(&automation.trigger).unwrap_or(Value::Null),
        run_mode: Some(run_mode_token(automation.execution.run_mode()).to_owned()),
        execution: serde_json::to_value(&automation.execution).unwrap_or(Value::Null),
        origin: Some(origin_token(automation.origin).to_owned()),
        created_by_thread_id: automation
            .created_by_thread_id
            .as_ref()
            .map(ToString::to_string),
        next_run_at: automation.next_run_at,
        last_run_at: automation.last_run_at,
        run_count: automation.run_count,
        consecutive_failures: automation.consecutive_failures,
        last_run_status: automation
            .last_run_status
            .map(|status| run_status_token(status).to_owned()),
        last_run_thread_id: automation
            .last_run_thread_id
            .as_ref()
            .map(ToString::to_string),
        last_error: automation.last_error.clone(),
        created_at: automation.created_at_ms,
        updated_at: automation.updated_at_ms,
    }
}

/// The stored token for an origin.
fn origin_token(origin: AutomationOrigin) -> &'static str {
    match origin {
        AutomationOrigin::Human => "human",
        AutomationOrigin::App => "app",
        AutomationOrigin::Agent => "agent",
    }
}

/// Decodes a stored run, or `None` when this build cannot read it.
///
/// A run row has no problem representation in the contract — the run list is
/// an array of runs and nothing else — so an unreadable row is left out of the
/// listing. It stays in the snapshot verbatim, so the data survives a build
/// that can read it again.
fn decode_run(row: &StoredAutomationRun) -> Option<AutomationRun> {
    let id: AutomationRunId = row.id.parse().ok()?;
    let automation_id: AutomationId = row.automation_id.parse().ok()?;
    let run_mode = match row.run_mode.as_str() {
        "agent" => AutomationRunMode::Agent,
        "script" => AutomationRunMode::Script,
        _ => return None,
    };
    let status = match row.status.as_str() {
        "running" => AutomationRunStatus::Running,
        "succeeded" => AutomationRunStatus::Succeeded,
        "failed" => AutomationRunStatus::Failed,
        "skipped" => AutomationRunStatus::Skipped,
        _ => return None,
    };
    let trigger = match row.trigger.as_str() {
        "schedule" => AutomationRunTrigger::Schedule,
        "manual" => AutomationRunTrigger::Manual,
        _ => return None,
    };
    let thread_id = match &row.thread_id {
        None => None,
        Some(value) => Some(value.parse::<ThreadId>().ok()?),
    };
    Some(AutomationRun {
        id,
        automation_id,
        run_mode,
        thread_id,
        status,
        trigger,
        skip_reason: row.skip_reason.clone(),
        error: row.error.clone(),
        output: row.output.clone(),
        exit_code: row.exit_code,
        idempotency_key: row.idempotency_key.clone(),
        scheduled_for: row.scheduled_for,
        started_at: row.started_at,
        finished_at: row.finished_at,
    })
}

/// Encodes a decoded run back into its stored form.
fn encode_run(run: &AutomationRun) -> StoredAutomationRun {
    StoredAutomationRun {
        id: run.id.to_string(),
        automation_id: run.automation_id.to_string(),
        run_mode: run_mode_token(run.run_mode).to_owned(),
        thread_id: run.thread_id.as_ref().map(ToString::to_string),
        status: run_status_token(run.status).to_owned(),
        trigger: run_trigger_token(run.trigger).to_owned(),
        skip_reason: run.skip_reason.clone(),
        error: run.error.clone(),
        output: run.output.clone(),
        exit_code: run.exit_code,
        idempotency_key: run.idempotency_key.clone(),
        scheduled_for: run.scheduled_for,
        started_at: run.started_at,
        finished_at: run.finished_at,
    }
}

/// Decodes a stored thread mark.
fn decode_thread_mark(row: &StoredAutomationThreadMark) -> Option<AutomationThreadMark> {
    Some(AutomationThreadMark {
        thread_id: row.thread_id.parse().ok()?,
        automation_id: row.automation_id.parse().ok()?,
        run_id: row.run_id.parse().ok()?,
        created_at_ms: row.created_at,
    })
}

/// Encodes a thread mark for storage.
fn encode_thread_mark(mark: &AutomationThreadMark) -> StoredAutomationThreadMark {
    StoredAutomationThreadMark {
        thread_id: mark.thread_id.to_string(),
        automation_id: mark.automation_id.to_string(),
        run_id: mark.run_id.to_string(),
        created_at: mark.created_at_ms,
    }
}

/// Projects a stored row the way `automationReadResultSchema` does.
fn read_result(row: &StoredAutomation) -> AutomationReadResult {
    match decode(row) {
        Ok(automation) if automation.is_missing_agent_prompt() => {
            AutomationReadResult::MissingAgentPrompt(MissingPromptAutomation {
                automation: automation.response(),
                problem: AutomationReadProblem::MissingAgentPrompt,
            })
        }
        Ok(automation) => AutomationReadResult::Automation(automation.response()),
        Err(()) => AutomationReadResult::InvalidStoredData(UnreadableAutomation {
            id: row.id.clone(),
            project_id: row.project_id.clone(),
            name: row.name.clone(),
            problem: AutomationReadProblem::InvalidStoredData,
        }),
    }
}

/* ------------------------------------------------------------------ */
/* State                                                               */
/* ------------------------------------------------------------------ */

/// The durable automation payload, as stored in the domain snapshot.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AutomationState {
    /// Payload version, `AUTOMATIONS_VERSION` for this build.
    #[serde(default)]
    pub version: u32,
    /// Every automation row.
    #[serde(default)]
    pub automations: Vec<StoredAutomation>,
    /// Every run row.
    #[serde(default)]
    pub runs: Vec<StoredAutomationRun>,
    /// Every thread mark.
    #[serde(default)]
    pub thread_marks: Vec<StoredAutomationThreadMark>,
}

impl AutomationState {
    /// A payload at this build's version.
    pub fn current() -> Self {
        Self {
            version: AUTOMATIONS_VERSION,
            ..Self::default()
        }
    }

    /// Drops duplicate rows, keeping the first occurrence.
    ///
    /// Ids are the identity of a row, and a snapshot that carries two rows for
    /// one id is not a state any command path can produce. Keeping the first
    /// makes the restored state deterministic instead of letting the answer
    /// depend on which duplicate a scan reaches first.
    fn deduplicate(&mut self) {
        let mut seen = std::collections::HashSet::new();
        self.automations.retain(|row| seen.insert(row.id.clone()));
        let mut seen = std::collections::HashSet::new();
        self.runs.retain(|row| seen.insert(row.id.clone()));
        let mut seen = std::collections::HashSet::new();
        self.thread_marks
            .retain(|row| seen.insert(row.thread_id.clone()));
    }
}

/// The in-memory automation rows behind one mutex.
///
/// The durable snapshot is written by the HTTP layer after each successful
/// mutation, the way a settings write is: an automation that a client created
/// is on disk before the response reaches it.
#[derive(Debug)]
pub struct AutomationsRegistry {
    inner: std::sync::Mutex<AutomationState>,
}

impl Default for AutomationsRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl AutomationsRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self {
            inner: std::sync::Mutex::new(AutomationState::current()),
        }
    }

    /// A stable copy for a snapshot.
    pub fn export(&self) -> AutomationState {
        self.lock().clone()
    }

    /// Restores a payload, applying the version policy.
    ///
    /// * version 0 (written before the field existed) is upgraded in place: the
    ///   rows keep whatever they have and gain this build's defaults, which is
    ///   the same treatment an older build's additive fields get.
    /// * a newer version is **not interpreted**. The workspace starts with no
    ///   automations rather than reading another build's representation as if
    ///   it were its own — the same choice the settings payload makes, and the
    ///   reason both carry a version. Nothing is deleted until the next
    ///   snapshot write replaces the payload, which is what running an older
    ///   binary against a newer snapshot means.
    pub fn restore(&self, mut state: AutomationState) {
        if state.version == 0 {
            state.version = AUTOMATIONS_VERSION;
        }
        if state.version > AUTOMATIONS_VERSION {
            eprintln!(
                "loom-server: automation payload version {} is newer than this build's {}; \
                 starting with no automations rather than reading an unknown representation",
                state.version, AUTOMATIONS_VERSION
            );
            *self.lock() = AutomationState::current();
            return;
        }
        state.deduplicate();
        *self.lock() = state;
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, AutomationState> {
        self.inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    /// Resolves a project-scoped automation row.
    fn row<'a>(
        state: &'a AutomationState,
        project_id: &str,
        automation_id: &str,
    ) -> Option<&'a StoredAutomation> {
        state
            .automations
            .iter()
            .find(|row| row.id == automation_id && row.project_id == project_id)
    }

    /// Every row of a project, newest first.
    pub fn list(&self, project_id: &str) -> Vec<StoredAutomation> {
        let state = self.lock();
        let mut rows: Vec<StoredAutomation> = state
            .automations
            .iter()
            .filter(|row| row.project_id == project_id)
            .cloned()
            .collect();
        sort_automations(&mut rows);
        rows
    }

    /// Every row, newest first.
    pub fn overview(&self) -> Vec<StoredAutomation> {
        let state = self.lock();
        let mut rows = state.automations.clone();
        sort_automations(&mut rows);
        rows
    }

    /// One row of a project.
    pub fn get(&self, project_id: &str, automation_id: &str) -> Option<StoredAutomation> {
        let state = self.lock();
        Self::row(&state, project_id, automation_id).cloned()
    }

    /// Creates an automation.
    pub fn create(
        &self,
        project_id: ProjectId,
        new: NewAutomation,
        now_ms: u64,
    ) -> Result<Automation, AutomationError> {
        let mut state = self.lock();
        if let Some(thread_id) = &new.created_by_thread_id {
            if is_automation_spawned_thread(&state, thread_id) {
                return Err(AutomationError::invalid_fields(
                    "createdByThreadId",
                    "names a thread an automation produced; automations cannot nest that way",
                ));
            }
        }
        let automation = Automation::create(AutomationId::mint(), project_id, new, now_ms)?;
        state.automations.push(encode(&automation));
        Ok(automation)
    }

    /// Applies a partial update.
    pub fn update(
        &self,
        project_id: &str,
        automation_id: &str,
        patch: &AutomationUpdate,
        now_ms: u64,
    ) -> Result<Automation, AutomationError> {
        let mut state = self.lock();
        let position = position_of(&state, project_id, automation_id)?;
        let mut automation = decode_for_write(&state.automations[position], "updated")?;
        require_repairable(&automation, patch)?;
        automation.update(patch, now_ms)?;
        state.automations[position] = encode(&automation);
        Ok(automation)
    }

    /// Deletes an automation and everything owned by it.
    pub fn delete(&self, project_id: &str, automation_id: &str) -> Result<(), AutomationError> {
        let mut state = self.lock();
        let position = position_of(&state, project_id, automation_id)?;
        state.automations.remove(position);
        state.runs.retain(|row| row.automation_id != automation_id);
        state
            .thread_marks
            .retain(|row| row.automation_id != automation_id);
        Ok(())
    }

    /// Pauses or resumes an automation.
    pub fn set_enabled(
        &self,
        project_id: &str,
        automation_id: &str,
        enabled: bool,
        now_ms: u64,
    ) -> Result<Automation, AutomationError> {
        let mut state = self.lock();
        let position = position_of(&state, project_id, automation_id)?;
        let mut automation = require_readable(
            &state.automations[position],
            if enabled { "resumed" } else { "paused" },
        )?;
        if enabled {
            automation.resume(now_ms)?;
        } else {
            automation.pause(now_ms);
        }
        state.automations[position] = encode(&automation);
        Ok(automation)
    }

    /// Creates the run row a manual trigger produces.
    ///
    /// Two requests cannot produce two runs of one automation: a repeated
    /// idempotency key returns the run it created, and a run that is still
    /// running is returned instead of starting a second one. Both are recorded
    /// the same way upstream does it — as a lookup over the run rows.
    pub fn start_manual_run(
        &self,
        project_id: &str,
        automation_id: &str,
        idempotency_key: Option<String>,
        now_ms: u64,
    ) -> Result<(AutomationRun, bool), AutomationError> {
        let mut state = self.lock();
        let position = position_of(&state, project_id, automation_id)?;
        let automation = require_readable(&state.automations[position], "run")?;
        if let Some(key) = &idempotency_key {
            validate_idempotency_key(key)?;
            if let Some(existing) = state
                .runs
                .iter()
                .filter(|row| row.automation_id == automation_id)
                .filter(|row| row.idempotency_key.as_deref() == Some(key.as_str()))
                .find_map(decode_run)
            {
                return Ok((existing, true));
            }
        }
        if let Some(running) = state
            .runs
            .iter()
            .filter(|row| row.automation_id == automation_id)
            .filter_map(decode_run)
            .find(|run| run.is_running())
        {
            return Ok((running, true));
        }
        let run = AutomationRun::start_manual(
            AutomationRunId::mint(),
            automation.id,
            automation.execution.run_mode(),
            idempotency_key,
            now_ms,
        );
        state.runs.push(encode_run(&run));
        Ok((run, false))
    }

    /// One page of an automation's run history, newest first.
    pub fn runs(
        &self,
        project_id: &str,
        automation_id: &str,
        limit: u32,
        cursor: Option<RunCursor>,
    ) -> Result<(Vec<AutomationRun>, bool), AutomationError> {
        validate_runs_limit(limit)?;
        let state = self.lock();
        position_of(&state, project_id, automation_id)?;
        let mut runs: Vec<AutomationRun> = state
            .runs
            .iter()
            .filter(|row| row.automation_id == automation_id)
            .filter_map(decode_run)
            .collect();
        runs.sort_by(|left, right| {
            right
                .started_at
                .cmp(&left.started_at)
                .then_with(|| right.id.to_string().cmp(&left.id.to_string()))
        });
        if let Some(cursor) = &cursor {
            runs.retain(|run| {
                run.started_at < cursor.started_at
                    || (run.started_at == cursor.started_at && run.id.to_string() < cursor.id)
            });
        }
        let more = runs.len() > limit as usize;
        runs.truncate(limit as usize);
        Ok((runs, more))
    }

    /// Records that a thread was produced by an automation.
    pub fn mark_thread(&self, mark: AutomationThreadMark) {
        let mut state = self.lock();
        let encoded = encode_thread_mark(&mark);
        match state
            .thread_marks
            .iter_mut()
            .find(|row| row.thread_id == encoded.thread_id)
        {
            Some(existing) => *existing = encoded,
            None => state.thread_marks.push(encoded),
        }
    }

    /// Whether a thread was produced by an automation.
    pub fn thread_mark(&self, thread_id: &ThreadId) -> Option<AutomationThreadMark> {
        let state = self.lock();
        state
            .thread_marks
            .iter()
            .filter(|row| row.thread_id == thread_id.to_string())
            .find_map(decode_thread_mark)
    }
}

/// Whether a thread is one an automation produced.
///
/// Two sources, exactly as upstream keeps two: the durable thread marks, and
/// the run history — a run that already names the thread is proof enough, and
/// it is what makes the check work for a run whose mark was never written.
fn is_automation_spawned_thread(state: &AutomationState, thread_id: &ThreadId) -> bool {
    let thread = thread_id.to_string();
    if state.thread_marks.iter().any(|row| row.thread_id == thread) {
        return true;
    }
    state
        .runs
        .iter()
        .filter_map(decode_run)
        .any(|run| run.thread_id.as_ref() == Some(thread_id))
}

/// Orders automations the way both listings do: newest first, then by id.
fn sort_automations(rows: &mut [StoredAutomation]) {
    rows.sort_by(|left, right| {
        right
            .created_at
            .cmp(&left.created_at)
            .then_with(|| right.id.cmp(&left.id))
    });
}

/// Finds a project-scoped row's position, or reports it missing.
fn position_of(
    state: &AutomationState,
    project_id: &str,
    automation_id: &str,
) -> Result<usize, AutomationError> {
    state
        .automations
        .iter()
        .position(|row| row.id == automation_id && row.project_id == project_id)
        .ok_or_else(|| {
            AutomationError::NotFound(format!(
                "automation {automation_id} is not known in project {project_id}"
            ))
        })
}

/// Decodes a row for a write operation, or reports why it cannot be used.
fn decode_for_write(
    row: &StoredAutomation,
    operation: &str,
) -> Result<Automation, AutomationError> {
    decode(row).map_err(|()| {
        AutomationError::Conflict(format!(
            "automation {:?} has invalid stored data and cannot be {operation}; delete it and \
             recreate it",
            row.name
        ))
    })
}

/// Decodes a row for an operation that cannot repair it, so a legacy empty
/// agent prompt blocks the write until an update supplies one.
fn require_readable(
    row: &StoredAutomation,
    operation: &str,
) -> Result<Automation, AutomationError> {
    let automation = decode_for_write(row, operation)?;
    if automation.is_missing_agent_prompt() {
        return Err(AutomationError::Conflict(format!(
            "automation {:?} requires a prompt before it can be {operation}; edit it and add a \
             prompt first",
            automation.name
        )));
    }
    Ok(automation)
}

/// Whether an update repairs a legacy empty prompt instead of leaving it.
///
/// A row that reads but has no prompt is frozen for every write except the one
/// that supplies a prompt, so a client cannot rename, retarget or pause an
/// automation that could never run.
fn require_repairable(
    automation: &Automation,
    patch: &AutomationUpdate,
) -> Result<(), AutomationError> {
    if !automation.is_missing_agent_prompt() {
        return Ok(());
    }
    let repairs = match (&patch.execution, &patch.agent) {
        (Some(AutomationExecution::Agent(agent)), _) => !agent.prompt.is_empty(),
        (_, Some(agent)) => agent
            .prompt
            .as_deref()
            .is_some_and(|prompt| !prompt.is_empty()),
        _ => false,
    };
    if repairs {
        return Ok(());
    }
    Err(AutomationError::Conflict(format!(
        "automation {:?} requires a prompt before other fields can be updated; edit it and add a \
         prompt first",
        automation.name
    )))
}

/* ------------------------------------------------------------------ */
/* Errors                                                             */
/* ------------------------------------------------------------------ */

/// A rejected automation operation.
#[derive(Debug, PartialEq, Eq)]
pub enum AutomationError {
    /// The project or automation does not exist.
    NotFound(String),
    /// The request was well-formed but the stored row cannot be used as it is.
    Conflict(String),
    /// A field the request named is not usable.
    Invalid { field: &'static str, reason: String },
}

impl AutomationError {
    fn invalid_fields(field: &'static str, reason: impl Into<String>) -> Self {
        Self::Invalid {
            field,
            reason: reason.into(),
        }
    }
}

impl From<loom_domain::DomainError> for AutomationError {
    fn from(error: loom_domain::DomainError) -> Self {
        match error {
            loom_domain::DomainError::InvalidField { field, reason } => {
                Self::Invalid { field, reason }
            }
            other => Self::Invalid {
                field: "automation",
                reason: other.to_string(),
            },
        }
    }
}

fn api_error(status: StatusCode, code: &'static str, message: impl Into<String>) -> Response {
    (
        status,
        Json(json!({ "code": code, "message": message.into() })),
    )
        .into_response()
}

/// Maps an operation failure onto the API's uniform error shape.
fn error_response(error: AutomationError) -> Response {
    match error {
        AutomationError::NotFound(message) => {
            api_error(StatusCode::NOT_FOUND, "not_found", message)
        }
        AutomationError::Conflict(message) => api_error(StatusCode::CONFLICT, "conflict", message),
        AutomationError::Invalid { field, reason } => api_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            format!("{field} {reason}"),
        ),
    }
}

/// Writes the snapshot after a successful mutation.
fn persist(state: &AppState) -> Result<(), Response> {
    state.snapshot().map_err(|error| {
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            format!("automations could not be persisted: {error}"),
        )
    })
}

/* ------------------------------------------------------------------ */
/* Cursors                                                             */
/* ------------------------------------------------------------------ */

/// A position in an automation's run history.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunCursor {
    /// The `startedAt` of the last row of the previous page.
    started_at: u64,
    /// That row's id.
    id: String,
}

impl RunCursor {
    /// Encodes a cursor as base64url over `"<startedAt>:<id>"`.
    ///
    /// The format is the reference implementation's, so a cursor issued by a
    /// client that stored one still means the same page. It is opaque to
    /// clients either way.
    pub fn encode(started_at: u64, id: &str) -> String {
        base64url_encode(format!("{started_at}:{id}").as_bytes())
    }

    /// Decodes a cursor, or reports it unusable.
    pub fn decode(raw: &str) -> Result<Self, AutomationError> {
        let bytes = base64url_decode(raw).ok_or_else(|| {
            AutomationError::invalid_fields("cursor", "is not a valid run cursor")
        })?;
        let text = String::from_utf8(bytes)
            .map_err(|_| AutomationError::invalid_fields("cursor", "is not a valid run cursor"))?;
        let (started_at, id) = text.split_once(':').ok_or_else(|| {
            AutomationError::invalid_fields("cursor", "is not a valid run cursor")
        })?;
        let started_at: u64 = started_at
            .parse()
            .map_err(|_| AutomationError::invalid_fields("cursor", "is not a valid run cursor"))?;
        if id.is_empty() {
            return Err(AutomationError::invalid_fields(
                "cursor",
                "is not a valid run cursor",
            ));
        }
        Ok(Self {
            started_at,
            id: id.to_owned(),
        })
    }
}

const BASE64URL_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// Base64url without padding, hand-rolled so the crate needs no dependency for
/// one opaque cursor.
fn base64url_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let mut buffer = [0u8; 3];
        buffer[..chunk.len()].copy_from_slice(chunk);
        let group = u32::from(buffer[0]) << 16 | u32::from(buffer[1]) << 8 | u32::from(buffer[2]);
        out.push(BASE64URL_ALPHABET[(group >> 18) as usize & 0x3f] as char);
        out.push(BASE64URL_ALPHABET[(group >> 12) as usize & 0x3f] as char);
        if chunk.len() > 1 {
            out.push(BASE64URL_ALPHABET[(group >> 6) as usize & 0x3f] as char);
        }
        if chunk.len() > 2 {
            out.push(BASE64URL_ALPHABET[group as usize & 0x3f] as char);
        }
    }
    out
}

/// Decodes base64url without padding, rejecting anything malformed.
fn base64url_decode(raw: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(raw.len() * 3 / 4);
    let mut group = 0u32;
    let mut bits = 0u32;
    for byte in raw.bytes() {
        let value = BASE64URL_ALPHABET
            .iter()
            .position(|candidate| *candidate == byte)? as u32;
        group = (group << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((group >> bits) as u8);
        }
    }
    // A trailing group that does not fill a byte must be zero-padded, which is
    // what an encoder produces and what a truncated cursor would not.
    if bits > 0 && group & ((1 << bits) - 1) != 0 {
        return None;
    }
    Some(out)
}

/* ------------------------------------------------------------------ */
/* HTTP handlers                                                       */
/* ------------------------------------------------------------------ */

/// `automations_overview`
pub async fn overview(State(state): State<AppState>) -> Response {
    let entries: Vec<Value> = state
        .automations
        .overview()
        .into_iter()
        .filter_map(|row| {
            let project = state
                .registry
                .project(&row.project_id.parse::<ProjectId>().ok()?)?;
            // A deleted project is a tombstone: its automations are not part of
            // what a client lists, exactly like the project itself.
            if project.is_deleted() {
                return None;
            }
            Some(json!({
                "automation": read_result(&row),
                "project": { "id": project.id.to_string(), "name": project.name },
            }))
        })
        .collect();
    Json(json!({ "automations": entries })).into_response()
}

/// `automations_list`
pub async fn list(State(state): State<AppState>, Path(project_id): Path<String>) -> Response {
    if let Err(response) = require_project(&state, &project_id) {
        return response;
    }
    let entries: Vec<AutomationReadResult> = state
        .automations
        .list(&project_id)
        .iter()
        .map(read_result)
        .collect();
    Json(entries).into_response()
}

/// `automations_get`
pub async fn get(
    State(state): State<AppState>,
    Path((project_id, automation_id)): Path<(String, String)>,
) -> Response {
    if let Err(response) = require_project(&state, &project_id) {
        return response;
    }
    if let Err(response) = require_automation_id(&automation_id) {
        return response;
    }
    match state.automations.get(&project_id, &automation_id) {
        Some(row) => Json(read_result(&row)).into_response(),
        None => not_found(&project_id, &automation_id),
    }
}

/// `automations_create`
pub async fn create(
    State(state): State<AppState>,
    Path(project_id): Path<String>,
    Json(request): Json<NewAutomation>,
) -> Response {
    let project_id = match require_project(&state, &project_id) {
        Ok(project_id) => project_id,
        Err(response) => return response,
    };
    let now = loom_relay::now_ms();
    let automation = match state.automations.create(project_id, request, now) {
        Ok(automation) => automation,
        Err(error) => return error_response(error),
    };
    if let Err(response) = persist(&state) {
        return response;
    }
    (StatusCode::CREATED, Json(json!(automation.response()))).into_response()
}

/// `automations_update`
pub async fn update(
    State(state): State<AppState>,
    Path((project_id, automation_id)): Path<(String, String)>,
    Json(patch): Json<AutomationUpdate>,
) -> Response {
    if let Err(response) = require_project(&state, &project_id) {
        return response;
    }
    if let Err(response) = require_automation_id(&automation_id) {
        return response;
    }
    let now = loom_relay::now_ms();
    let automation = match state
        .automations
        .update(&project_id, &automation_id, &patch, now)
    {
        Ok(automation) => automation,
        Err(error) => return error_response(error),
    };
    if let Err(response) = persist(&state) {
        return response;
    }
    Json(json!(automation.response())).into_response()
}

/// `automations_delete`
pub async fn delete(
    State(state): State<AppState>,
    Path((project_id, automation_id)): Path<(String, String)>,
) -> Response {
    if let Err(response) = require_project(&state, &project_id) {
        return response;
    }
    if let Err(response) = require_automation_id(&automation_id) {
        return response;
    }
    if let Err(error) = state.automations.delete(&project_id, &automation_id) {
        return error_response(error);
    }
    if let Err(response) = persist(&state) {
        return response;
    }
    Json(json!({ "ok": true })).into_response()
}

/// `automations_pause`
pub async fn pause(
    State(state): State<AppState>,
    Path((project_id, automation_id)): Path<(String, String)>,
) -> Response {
    set_enabled(state, project_id, automation_id, false).await
}

/// `automations_resume`
pub async fn resume(
    State(state): State<AppState>,
    Path((project_id, automation_id)): Path<(String, String)>,
) -> Response {
    set_enabled(state, project_id, automation_id, true).await
}

async fn set_enabled(
    state: AppState,
    project_id: String,
    automation_id: String,
    enabled: bool,
) -> Response {
    if let Err(response) = require_project(&state, &project_id) {
        return response;
    }
    if let Err(response) = require_automation_id(&automation_id) {
        return response;
    }
    let now = loom_relay::now_ms();
    let automation = match state
        .automations
        .set_enabled(&project_id, &automation_id, enabled, now)
    {
        Ok(automation) => automation,
        Err(error) => return error_response(error),
    };
    if let Err(response) = persist(&state) {
        return response;
    }
    Json(json!(automation.response())).into_response()
}

/// The body of a manual run request.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RunAutomationRequest {
    /// The client's deduplication key.
    #[serde(default)]
    idempotency_key: Option<String>,
}

/// `automations_run`
pub async fn run(
    State(state): State<AppState>,
    Path((project_id, automation_id)): Path<(String, String)>,
    body: Option<Json<RunAutomationRequest>>,
) -> Response {
    if let Err(response) = require_project(&state, &project_id) {
        return response;
    }
    if let Err(response) = require_automation_id(&automation_id) {
        return response;
    }
    let request = body.map(|Json(request)| request).unwrap_or_default();
    let now = loom_relay::now_ms();
    let (run, deduped) = match state.automations.start_manual_run(
        &project_id,
        &automation_id,
        request.idempotency_key,
        now,
    ) {
        Ok(outcome) => outcome,
        Err(error) => return error_response(error),
    };
    if let Err(response) = persist(&state) {
        return response;
    }
    // A deduplicated request — a repeated idempotency key, or a run already in
    // flight — created nothing, so it answers `200` rather than `201`: a client
    // can tell whether it started work without comparing run ids.
    let status = if deduped {
        StatusCode::OK
    } else {
        StatusCode::CREATED
    };
    (status, Json(json!({ "run": run.response() }))).into_response()
}

/// Query parameters of a run listing.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RunsQuery {
    /// Page size.
    #[serde(default)]
    limit: Option<String>,
    /// Opaque cursor from a previous page.
    #[serde(default)]
    cursor: Option<String>,
}

/// `automations_runs`
pub async fn runs(
    State(state): State<AppState>,
    Path((project_id, automation_id)): Path<(String, String)>,
    Query(query): Query<RunsQuery>,
) -> Response {
    if let Err(response) = require_project(&state, &project_id) {
        return response;
    }
    if let Err(response) = require_automation_id(&automation_id) {
        return response;
    }
    let limit = match query.limit.as_deref() {
        None => loom_domain::automation::AUTOMATION_RUNS_LIMIT_DEFAULT,
        Some(raw) => match raw.parse::<u32>() {
            Ok(limit) => limit,
            Err(_) => {
                return api_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request",
                    "limit must be a positive integer",
                )
            }
        },
    };
    let cursor = match query.cursor.as_deref() {
        None => None,
        Some(raw) => match RunCursor::decode(raw) {
            Ok(cursor) => Some(cursor),
            Err(error) => return error_response(error),
        },
    };
    let (runs, more) = match state
        .automations
        .runs(&project_id, &automation_id, limit, cursor)
    {
        Ok(page) => page,
        Err(error) => return error_response(error),
    };
    let next_cursor = if more {
        runs.last()
            .map(|run| RunCursor::encode(run.started_at, &run.id.to_string()))
    } else {
        None
    };
    let runs: Vec<Value> = runs.iter().map(|run| json!(run.response())).collect();
    Json(json!({ "runs": runs, "nextCursor": next_cursor })).into_response()
}

/// Resolves a project path parameter, rejecting unknown and deleted projects.
fn require_project(state: &AppState, raw: &str) -> Result<ProjectId, Response> {
    let project_id: ProjectId = raw.parse().map_err(|error| {
        api_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            format!("projectId {raw:?} is not a valid project id: {error}"),
        )
    })?;
    let project = state.registry.project(&project_id).ok_or_else(|| {
        api_error(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("project {project_id} is not known"),
        )
    })?;
    if project.is_deleted() {
        return Err(api_error(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("project {project_id} has been deleted"),
        ));
    }
    Ok(project_id)
}

/// Rejects an automation id that is not shaped like one.
fn require_automation_id(raw: &str) -> Result<(), Response> {
    raw.parse::<AutomationId>().map(|_| ()).map_err(|error| {
        api_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            format!("automationId {raw:?} is not a valid automation id: {error}"),
        )
    })
}

fn not_found(project_id: &str, automation_id: &str) -> Response {
    api_error(
        StatusCode::NOT_FOUND,
        "not_found",
        format!("automation {automation_id} is not known in project {project_id}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use loom_domain::automation::{
        AgentEnvironment, AgentExecution, AgentExecutionUpdate, AutomationExecution,
        AutomationRunStatus, AutomationTrigger, PermissionMode, ScriptExecution, ScriptInterpreter,
    };
    use loom_domain::ReasoningLevel;

    fn now() -> u64 {
        1_800_000_000_000
    }

    fn project_id() -> ProjectId {
        ProjectId::mint()
    }

    fn agent_execution() -> AutomationExecution {
        AutomationExecution::Agent(AgentExecution {
            prompt: "summarise".into(),
            provider_id: "pi".into(),
            model: "pi/default".into(),
            reasoning_level: ReasoningLevel::Medium,
            service_tier: None,
            permission_mode: PermissionMode::Auto,
            environment: AgentEnvironment::ProjectDefault,
            target_thread_id: None,
        })
    }

    fn new_automation(name: &str) -> NewAutomation {
        NewAutomation {
            name: name.into(),
            enabled: true,
            trigger: AutomationTrigger::Schedule {
                cron: "0 9 * * *".into(),
                timezone: "UTC".into(),
            },
            execution: agent_execution(),
            origin: AutomationOrigin::Human,
            created_by_thread_id: None,
        }
    }

    #[test]
    fn a_row_round_trips_through_its_stored_form() {
        let registry = AutomationsRegistry::new();
        let project = project_id();
        let created = registry
            .create(project.clone(), new_automation("nightly"), now())
            .expect("creates");
        let row = registry
            .get(&project.to_string(), &created.id.to_string())
            .expect("stored");
        let decoded = decode(&row).expect("decodes");
        assert_eq!(decoded, created);
        assert_eq!(row.trigger_type.as_deref(), Some("schedule"));
        assert_eq!(row.run_mode.as_deref(), Some("agent"));
        assert_eq!(row.origin.as_deref(), Some("human"));
        assert_eq!(
            read_result(&row),
            AutomationReadResult::Automation(created.response())
        );
    }

    #[test]
    fn a_row_missing_an_additive_field_still_reads() {
        let registry = AutomationsRegistry::new();
        let project = project_id();
        let created = registry
            .create(project.clone(), new_automation("nightly"), now())
            .expect("creates");
        let row = registry
            .get(&project.to_string(), &created.id.to_string())
            .expect("stored");
        let mut value = serde_json::to_value(&row).expect("serializable");
        let object = value.as_object_mut().expect("an object");
        // A field a later build added: absent means "no failure yet".
        object.remove("consecutiveFailures");
        object.remove("lastRunStatus");
        let restored: StoredAutomation = serde_json::from_value(value).expect("tolerant rows");
        assert_eq!(restored.consecutive_failures, 0);
        assert!(matches!(
            read_result(&restored),
            AutomationReadResult::Automation(_)
        ));
    }

    #[test]
    fn a_row_with_a_trigger_the_discriminator_contradicts_is_a_read_problem() {
        let registry = AutomationsRegistry::new();
        let project = project_id();
        let created = registry
            .create(project.clone(), new_automation("nightly"), now())
            .expect("creates");
        let mut row = registry
            .get(&project.to_string(), &created.id.to_string())
            .expect("stored");
        row.trigger_type = Some("once".into());
        assert!(matches!(
            read_result(&row),
            AutomationReadResult::InvalidStoredData(problem)
                if problem.problem == AutomationReadProblem::InvalidStoredData
                && problem.id == created.id.to_string()
        ));
    }

    #[test]
    fn a_row_this_build_cannot_parse_at_all_is_a_read_problem() {
        let row = StoredAutomation {
            id: AutomationId::mint().to_string(),
            project_id: ProjectId::mint().to_string(),
            name: "damaged".into(),
            execution: json!({ "mode": "telepathy" }),
            ..StoredAutomation::default()
        };
        assert_eq!(
            read_result(&row),
            AutomationReadResult::InvalidStoredData(UnreadableAutomation {
                id: row.id.clone(),
                project_id: row.project_id.clone(),
                name: "damaged".into(),
                problem: AutomationReadProblem::InvalidStoredData,
            })
        );
    }

    #[test]
    fn a_row_that_violates_the_contract_rules_is_a_read_problem_too() {
        // It parses as an agent execution, but the contract would not accept
        // it: the provider is empty. Only the legacy *prompt* is repairable.
        let row = StoredAutomation {
            id: AutomationId::mint().to_string(),
            project_id: ProjectId::mint().to_string(),
            name: "empty provider".into(),
            trigger_type: Some("schedule".into()),
            trigger: json!({ "triggerType": "schedule", "cron": "0 9 * * *", "timezone": "UTC" }),
            run_mode: Some("agent".into()),
            execution: json!({
                "mode": "agent",
                "prompt": "do the thing",
                "providerId": "",
                "model": "pi/default",
                "reasoningLevel": "medium",
                "permissionMode": "auto",
                "environment": { "type": "project-default" }
            }),
            origin: Some("human".into()),
            ..StoredAutomation::default()
        };
        assert!(matches!(
            read_result(&row),
            AutomationReadResult::InvalidStoredData(_)
        ));
    }

    #[test]
    fn a_legacy_empty_prompt_row_reads_as_a_problem_and_freezes_writes() {
        let registry = AutomationsRegistry::new();
        let project = project_id();
        let created = registry
            .create(project.clone(), new_automation("nightly"), now())
            .expect("creates");
        let mut row = registry
            .get(&project.to_string(), &created.id.to_string())
            .expect("stored");
        row.execution["prompt"] = json!("");
        {
            // Hold the lock for the edit only: `AutomationsRegistry::lock` is
            // not reentrant, so anything that takes it again must run after
            // the guard is dropped.
            let mut state = registry.lock();
            state.automations[0] = row.clone();
        }

        assert!(matches!(
            read_result(&row),
            AutomationReadResult::MissingAgentPrompt(problem)
                if problem.problem == AutomationReadProblem::MissingAgentPrompt
                && problem.automation.execution.is_missing_prompt()
        ));

        let rename = AutomationUpdate {
            name: Some("renamed".into()),
            ..Default::default()
        };
        assert!(matches!(
            registry.update(
                &project.to_string(),
                &created.id.to_string(),
                &rename,
                now()
            ),
            Err(AutomationError::Conflict(_))
        ));

        let repair = AutomationUpdate {
            agent: Some(AgentExecutionUpdate {
                prompt: Some("a real prompt".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        registry
            .update(
                &project.to_string(),
                &created.id.to_string(),
                &repair,
                now(),
            )
            .expect("a prompt repairs the row");
    }

    #[test]
    fn restoring_a_newer_payload_leaves_it_alone() {
        let registry = AutomationsRegistry::new();
        registry
            .create(project_id(), new_automation("nightly"), now())
            .expect("creates");

        let mut future = AutomationState::current();
        future.version = AUTOMATIONS_VERSION + 1;
        future.automations.push(StoredAutomation::default());
        registry.restore(future);
        assert!(registry.overview().is_empty());

        registry.restore(AutomationState::current());
        assert!(registry.overview().is_empty());
    }

    #[test]
    fn restoring_a_versionless_payload_upgrades_it_in_place() {
        let registry = AutomationsRegistry::new();
        let project = project_id();
        let created = registry
            .create(project.clone(), new_automation("nightly"), now())
            .expect("creates");
        let mut exported = registry.export();
        exported.version = 0;
        registry.restore(exported.clone());
        assert_eq!(
            registry.export(),
            AutomationState {
                version: AUTOMATIONS_VERSION,
                ..exported
            }
        );
        assert!(registry
            .get(&project.to_string(), &created.id.to_string())
            .is_some());
    }

    #[test]
    fn restoring_drops_duplicate_rows_deterministically() {
        let registry = AutomationsRegistry::new();
        let project = project_id();
        let created = registry
            .create(project.clone(), new_automation("nightly"), now())
            .expect("creates");
        let mut state = registry.export();
        let mut duplicate = state.automations[0].clone();
        duplicate.name = "duplicate".into();
        state.automations.push(duplicate);
        registry.restore(state);
        let rows = registry.list(&project.to_string());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, created.id.to_string());
        assert_eq!(rows[0].name, "nightly");
    }

    #[test]
    fn deleting_an_automation_takes_its_runs_and_marks_with_it() {
        let registry = AutomationsRegistry::new();
        let project = project_id();
        let created = registry
            .create(project.clone(), new_automation("nightly"), now())
            .expect("creates");
        let automation_id = created.id.to_string();
        let project = project.to_string();
        registry
            .start_manual_run(&project, &automation_id, None, now())
            .expect("starts");
        let run = registry
            .runs(&project, &automation_id, 10, None)
            .expect("lists")
            .0;
        assert_eq!(run.len(), 1);
        registry.mark_thread(AutomationThreadMark {
            thread_id: ThreadId::mint(),
            automation_id: created.id.clone(),
            run_id: run[0].id.clone(),
            created_at_ms: now(),
        });

        registry.delete(&project, &automation_id).expect("deletes");
        assert!(registry.get(&project, &automation_id).is_none());
        assert_eq!(registry.export().runs.len(), 0);
        assert_eq!(registry.export().thread_marks.len(), 0);
    }

    #[test]
    fn a_manual_run_is_single_flight_and_idempotent() {
        let registry = AutomationsRegistry::new();
        let project = project_id().to_string();
        let automation_id = registry
            .create(
                project.parse().expect("the fixture project id parses"),
                new_automation("nightly"),
                now(),
            )
            .expect("creates")
            .id
            .to_string();

        let (first, deduped) = registry
            .start_manual_run(&project, &automation_id, Some("key-1".into()), now())
            .expect("starts");
        assert!(!deduped);
        let (second, deduped) = registry
            .start_manual_run(&project, &automation_id, Some("key-1".into()), now() + 1)
            .expect("dedupes");
        assert!(deduped);
        assert_eq!(second.id, first.id);
        // A different key does not start a second run while one is in flight.
        let (third, deduped) = registry
            .start_manual_run(&project, &automation_id, Some("key-2".into()), now() + 2)
            .expect("single flight");
        assert!(deduped);
        assert_eq!(third.id, first.id);
        assert_eq!(registry.export().runs.len(), 1);
    }

    #[test]
    fn an_idempotency_key_longer_than_the_limit_is_rejected() {
        let registry = AutomationsRegistry::new();
        let project = project_id().to_string();
        let automation_id = registry
            .create(
                project.parse().expect("the fixture project id parses"),
                new_automation("nightly"),
                now(),
            )
            .expect("creates")
            .id
            .to_string();
        let key = "k".repeat(loom_domain::automation::AUTOMATION_IDEMPOTENCY_KEY_MAX_LENGTH + 1);
        assert!(matches!(
            registry.start_manual_run(&project, &automation_id, Some(key), now()),
            Err(AutomationError::Invalid {
                field: "idempotencyKey",
                ..
            })
        ));
    }

    #[test]
    fn an_automation_created_by_an_automation_thread_is_refused() {
        let registry = AutomationsRegistry::new();
        let project = project_id();
        let created = registry
            .create(project.clone(), new_automation("nightly"), now())
            .expect("creates");
        let thread_id = ThreadId::mint();
        registry.mark_thread(AutomationThreadMark {
            thread_id: thread_id.clone(),
            automation_id: created.id.clone(),
            run_id: AutomationRunId::mint(),
            created_at_ms: now(),
        });
        assert!(registry.thread_mark(&thread_id).is_some());

        let mut nested = new_automation("nested");
        nested.created_by_thread_id = Some(thread_id.clone());
        assert!(matches!(
            registry.create(project, nested, now()),
            Err(AutomationError::Invalid {
                field: "createdByThreadId",
                ..
            })
        ));
    }

    #[test]
    fn an_automation_created_by_an_ordinary_thread_is_accepted() {
        let registry = AutomationsRegistry::new();
        let mut nested = new_automation("nested");
        let thread_id = ThreadId::mint();
        nested.created_by_thread_id = Some(thread_id);
        registry
            .create(project_id(), nested, now())
            .expect("an ordinary thread is not an automation thread");
    }

    #[test]
    fn run_pages_are_ordered_and_cursored() {
        let registry = AutomationsRegistry::new();
        let project = project_id();
        let created = registry
            .create(project.clone(), new_automation("nightly"), now())
            .expect("creates");
        let project = project.to_string();
        let automation_id = created.id.to_string();
        // Three runs with distinct start times and a terminal status, so the
        // single-flight guard does not collapse them.
        {
            let mut state = registry.lock();
            for index in 0..3u64 {
                let mut run = AutomationRun::start_manual(
                    AutomationRunId::mint(),
                    created.id.clone(),
                    AutomationRunMode::Agent,
                    None,
                    now() + index,
                );
                run.status = AutomationRunStatus::Succeeded;
                run.finished_at = Some(now() + index + 1);
                state.runs.push(encode_run(&run));
            }
        }
        let (page, _) = registry
            .runs(&project, &automation_id, 2, None)
            .expect("lists");
        assert_eq!(page.len(), 2);
        assert!(page[0].started_at > page[1].started_at);

        let cursor = RunCursor::decode(&RunCursor::encode(
            page[1].started_at,
            &page[1].id.to_string(),
        ))
        .expect("round trips");
        let (next, more) = registry
            .runs(&project, &automation_id, 2, Some(cursor))
            .expect("lists");
        assert_eq!(next.len(), 1);
        assert!(!more);
        assert!(next[0].started_at < page[1].started_at);
    }

    #[test]
    fn a_cursor_that_is_not_a_cursor_is_rejected() {
        for raw in ["", "!!!!", "eA", "MTo", "MT"] {
            assert!(RunCursor::decode(raw).is_err(), "{raw} should be rejected");
        }
        let cursor = RunCursor::decode(&RunCursor::encode(1234, "arun_x")).expect("round trips");
        assert_eq!(
            cursor,
            RunCursor {
                started_at: 1234,
                id: "arun_x".into()
            }
        );
    }

    #[test]
    fn a_run_limit_outside_the_bounds_is_rejected() {
        let registry = AutomationsRegistry::new();
        let project = project_id();
        let created = registry
            .create(project.clone(), new_automation("nightly"), now())
            .expect("creates");
        let project = project.to_string();
        let automation_id = created.id.to_string();
        assert!(matches!(
            registry.runs(&project, &automation_id, 0, None),
            Err(AutomationError::Invalid { field: "limit", .. })
        ));
        assert!(matches!(
            registry.runs(
                &project,
                &automation_id,
                loom_domain::automation::AUTOMATION_RUNS_LIMIT_MAX + 1,
                None
            ),
            Err(AutomationError::Invalid { field: "limit", .. })
        ));
    }

    #[test]
    fn listings_are_ordered_newest_first_and_never_depend_on_insertion_order() {
        let registry = AutomationsRegistry::new();
        let project = project_id();
        let first = registry
            .create(project.clone(), new_automation("first"), now())
            .expect("creates");
        let second = registry
            .create(project.clone(), new_automation("second"), now() + 1)
            .expect("creates");
        let rows = registry.list(&project.to_string());
        assert_eq!(rows[0].id, second.id.to_string());
        assert_eq!(rows[1].id, first.id.to_string());
        let entries = registry.overview();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].id, second.id.to_string());
    }

    #[test]
    fn a_script_automation_stores_its_execution_and_mode() {
        let registry = AutomationsRegistry::new();
        let project = project_id();
        let mut new = new_automation("script");
        new.execution = AutomationExecution::Script(ScriptExecution {
            script: Some("echo hi".into()),
            script_file: None,
            interpreter: Some(ScriptInterpreter::Bash),
            timeout_ms: loom_domain::automation::AUTOMATION_SCRIPT_TIMEOUT_DEFAULT_MS,
            env: None,
        });
        let created = registry
            .create(project.clone(), new, now())
            .expect("creates");
        let row = registry
            .get(&project.to_string(), &created.id.to_string())
            .expect("stored");
        assert_eq!(row.run_mode.as_deref(), Some("script"));
        assert_eq!(row.execution["mode"], "script");
        assert_eq!(row.execution["timeoutMs"], 120_000);
        let decoded = decode(&row).expect("decodes");
        assert_eq!(decoded.execution.run_mode(), AutomationRunMode::Script);
    }

    #[test]
    fn a_project_scoped_lookup_does_not_leak_across_projects() {
        let registry = AutomationsRegistry::new();
        let owner = project_id();
        let other = project_id();
        let created = registry
            .create(owner.clone(), new_automation("nightly"), now())
            .expect("creates");
        assert!(registry
            .get(&other.to_string(), &created.id.to_string())
            .is_none());
        assert!(registry.list(&other.to_string()).is_empty());
        assert!(matches!(
            registry.delete(&other.to_string(), &created.id.to_string()),
            Err(AutomationError::NotFound(_))
        ));
    }

    #[test]
    fn a_trigger_the_contract_rejects_never_reaches_the_snapshot() {
        let registry = AutomationsRegistry::new();
        let mut new = new_automation("bad cron");
        new.trigger = AutomationTrigger::Schedule {
            cron: "0 9 * *".into(),
            timezone: "UTC".into(),
        };
        assert!(matches!(
            registry.create(project_id(), new, now()),
            Err(AutomationError::Invalid {
                field: "trigger.cron",
                ..
            })
        ));
        assert!(registry.export().automations.is_empty());
    }
}
