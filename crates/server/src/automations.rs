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
//! # What is here, and what is not
//!
//! The scheduler is here: [`AutomationsRegistry::sweep_due`] arms schedules,
//! claims the windows that have arrived, queues a run for each of them and
//! moves the schedule past the window it claimed — so a restart cannot replay
//! one. The run lifecycle is here too ([`AutomationsRegistry::start_run`],
//! [`AutomationsRegistry::close_run`], the failure policy that retries a
//! scheduled failure and pauses the automation after three).
//!
//! Execution is not: nothing in this server produces a thread or runs a script.
//! A queued run waits for the execution plane, and pausing abandons the ones
//! that never started. Realtime invalidation and the UI are the stages after
//! that.

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
    AutomationRunMode, AutomationRunOutcome, AutomationRunState, AutomationRunTrigger,
    AutomationThreadMark, AutomationTrigger, AutomationUpdate, MissingPromptAutomation,
    NewAutomation, UnreadableAutomation,
};
use loom_domain::schedule::Schedule;
use loom_domain::{AutomationId, AutomationRunId, ProjectId, ThreadId};

use crate::state::AppState;

/// The current automation payload version.
///
/// * **Version 1** is a payload from before the scheduler: run rows were only
///   ever `running` (nothing could claim them) and automations had no
///   `nextRunAt` for a cron schedule. Restoring one rewrites those rows to
///   `pending` — a queue entry nothing had picked up — and the sweep arms the
///   schedules it can evaluate. See [`migrate_payload`].
/// * **Version 0** is a payload written before the field existed: it loads with
///   the same additive defaults (`#[serde(default)]` on every row field) a
///   version-1 payload gets for a field added later.
/// * A *newer* version is not interpreted at all — see
///   [`AutomationsRegistry::restore`].
pub const AUTOMATIONS_VERSION: u32 = 2;

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
    /// The provider run this one became, once it was dispatched.
    #[serde(default)]
    pub provider_run_id: Option<String>,
    /// The machine a script run was dispatched to, so a cancel can reach it
    /// and a run whose host is gone can be reaped.
    #[serde(default)]
    pub host_id: Option<String>,
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
        Some(token) => Some(AutomationRunState::from_token(token).ok_or(())?),
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
            .map(|state| state.as_str().to_owned()),
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
    let state = AutomationRunState::from_token(&row.status)?;
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
        state,
        trigger,
        skip_reason: row.skip_reason.clone(),
        error: row.error.clone(),
        output: row.output.clone(),
        exit_code: row.exit_code,
        idempotency_key: row.idempotency_key.clone(),
        provider_run_id: row.provider_run_id.clone(),
        host_id: row.host_id.as_deref().and_then(|raw| raw.parse().ok()),
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
        status: run.state.as_str().to_owned(),
        trigger: run_trigger_token(run.trigger).to_owned(),
        skip_reason: run.skip_reason.clone(),
        error: run.error.clone(),
        output: run.output.clone(),
        exit_code: run.exit_code,
        idempotency_key: run.idempotency_key.clone(),
        provider_run_id: run.provider_run_id.clone(),
        host_id: run.host_id.as_ref().map(ToString::to_string),
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

/// Upgrades a payload to this build's version, in place.
///
/// The only step so far is version 1 → 2, the scheduler release. In version 1
/// nothing could claim a run, so a row stored as `running` was a queued
/// execution intent and becomes `pending` — which is what it always was.
/// Schedules written then have no `nextRunAt`; they are armed by the sweep
/// rather than here, because arming needs the clock and the zone database and
/// this runs before the server is serving.
fn migrate_payload(state: &mut AutomationState) {
    if state.version < 2 {
        for row in &mut state.runs {
            if row.status == "running" {
                row.status = AutomationRunState::Pending.as_str().to_owned();
            }
        }
    }
    state.version = AUTOMATIONS_VERSION;
}

/// What one sweep found and did, for the log line and for the tests.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SweepReport {
    /// Enabled automations whose window had arrived.
    pub due: usize,
    /// Runs queued by this sweep.
    pub claimed: usize,
    /// The projects those runs belong to, deduplicated, oldest first.
    ///
    /// The caller publishes one invalidation per project: a queued run is a
    /// change a client has to be told about, and the scheduler is the only
    /// producer that does not know whose automations it just moved.
    pub claimed_projects: Vec<ProjectId>,
    /// Enabled schedules that had no next instant and now have one.
    pub armed: usize,
    /// Due automations that already had work in flight and were left alone.
    pub in_flight: usize,
    /// Enabled schedules nothing could evaluate (bad expression or zone).
    pub unevaluable: usize,
    /// Rows this build cannot read at all.
    pub unreadable: usize,
    /// Schedules that fired their last possible window and disabled themselves.
    pub exhausted: usize,
}

impl SweepReport {
    /// Whether the sweep changed anything that has to be persisted.
    pub fn changed(&self) -> bool {
        self.claimed > 0 || self.armed > 0 || self.exhausted > 0
    }

    /// One line for the operator log, or `None` when there is nothing to say.
    ///
    /// The counters that mean "an automation is waiting on something" are
    /// reported even when nothing changed: a schedule that is due and blocked
    /// behind a run in flight, or one this build cannot evaluate, is the answer
    /// to "why did my automation not fire".
    pub fn diagnostic(&self) -> Option<String> {
        if !self.changed()
            && self.in_flight == 0
            && self.unevaluable == 0
            && self.unreadable == 0
            && self.due == 0
        {
            return None;
        }
        Some(format!(
            "automation sweep: {} due, {} queued, {} armed, {} waiting on a run in flight, {} \
             unevaluable, {} unreadable, {} exhausted",
            self.due,
            self.claimed,
            self.armed,
            self.in_flight,
            self.unevaluable,
            self.unreadable,
            self.exhausted
        ))
    }
}

/// Abandons the runs of one automation that have not started.
///
/// Only a pending run can be cancelled: a run the execution plane has already
/// started is not something this layer can interrupt, so it is left to finish
/// (or to be failed by the reconciler that owns in-flight work).
fn cancel_pending_runs(
    state: &mut AutomationState,
    automation_id: &str,
    reason: &str,
    now_ms: u64,
) -> Vec<AutomationRun> {
    let mut cancelled = Vec::new();
    for position in 0..state.runs.len() {
        if state.runs[position].automation_id != automation_id {
            continue;
        }
        let Some(mut run) = decode_run(&state.runs[position]) else {
            continue;
        };
        if run.state != AutomationRunState::Pending {
            continue;
        }
        let outcome = AutomationRunOutcome::Cancelled {
            reason: reason.to_owned(),
        };
        if run.finish(&outcome, now_ms).is_err() {
            continue;
        }
        state.runs[position] = encode_run(&run);
        cancelled.push(run);
    }
    cancelled
}

/// The run an automation already has in flight, oldest first.
fn in_flight_run(state: &AutomationState, automation_id: &str) -> Option<AutomationRun> {
    state
        .runs
        .iter()
        .filter(|row| row.automation_id == automation_id)
        .filter_map(decode_run)
        .filter(AutomationRun::is_in_flight)
        .min_by(|left, right| {
            left.started_at
                .cmp(&right.started_at)
                .then_with(|| left.id.to_string().cmp(&right.id.to_string()))
        })
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
    /// * version 0 (written before the field existed) and version 1 (written
    ///   before the scheduler) are upgraded in place by [`migrate_payload`].
    /// * a newer version is **not interpreted**. The workspace starts with no
    ///   automations rather than reading another build's representation as if
    ///   it were its own — the same choice the settings payload makes, and the
    ///   reason both carry a version. Nothing is deleted until the next
    ///   snapshot write replaces the payload, which is what running an older
    ///   binary against a newer snapshot means.
    pub fn restore(&self, mut state: AutomationState) {
        if state.version > AUTOMATIONS_VERSION {
            eprintln!(
                "loom-server: automation payload version {} is newer than this build's {}; \
                 starting with no automations rather than reading an unknown representation",
                state.version, AUTOMATIONS_VERSION
            );
            *self.lock() = AutomationState::current();
            return;
        }
        let migrating = state.version < AUTOMATIONS_VERSION;
        migrate_payload(&mut state);
        state.deduplicate();
        let pending = state
            .runs
            .iter()
            .filter(|row| row.status == AutomationRunState::Pending.as_str())
            .count();
        *self.lock() = state;
        if migrating {
            eprintln!(
                "loom-server: migrated the automation payload to version {AUTOMATIONS_VERSION} \
                 ({pending} queued runs kept)"
            );
        }
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

    /// One automation by id, decoded.
    ///
    /// The executor works from the decoded row: it needs the trigger and the
    /// execution, not the wire projection.
    pub fn automation(&self, automation_id: &AutomationId) -> Option<Automation> {
        let state = self.lock();
        state
            .automations
            .iter()
            .find(|row| row.id == automation_id.to_string())
            .and_then(|row| decode(row).ok())
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
    ///
    /// A pause also abandons what the automation had queued: leaving a queued
    /// run behind would let it run once more after the user stopped it, which
    /// is the opposite of what pausing promises. Resuming re-arms from *now*,
    /// so the windows that passed while it was paused are not replayed.
    pub fn set_enabled(
        &self,
        project_id: &str,
        automation_id: &str,
        enabled: bool,
        now_ms: u64,
    ) -> Result<(Automation, Vec<AutomationRun>), AutomationError> {
        let mut state = self.lock();
        let position = position_of(&state, project_id, automation_id)?;
        let mut automation = require_readable(
            &state.automations[position],
            if enabled { "resumed" } else { "paused" },
        )?;
        let cancelled = if enabled {
            automation.resume(now_ms)?;
            Vec::new()
        } else {
            automation.pause(now_ms);
            let cancelled = cancel_pending_runs(
                &mut state,
                automation_id,
                "cancelled: the automation was paused",
                now_ms,
            );
            if let Ok(mut stored) = decode(&state.automations[position]) {
                if let Some(last) = cancelled.last() {
                    stored.record_run_outcome(
                        &AutomationRunOutcome::Cancelled {
                            reason: last.skip_reason.clone().unwrap_or_default(),
                        },
                        last.trigger,
                        now_ms,
                    );
                }
                state.automations[position] = encode(&stored);
            }
            cancelled
        };
        state.automations[position] = encode(&automation);
        Ok((automation, cancelled))
    }

    /// Queues the run a manual trigger asks for.
    ///
    /// Two requests cannot produce two runs of one automation: a repeated
    /// idempotency key returns the run it created, and a run that is already in
    /// flight is returned instead of queueing a second one — the same rule the
    /// sweep obeys, so a manual trigger and a due window can never run side by
    /// side.
    pub fn queue_manual_run(
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
        if let Some(in_flight) = in_flight_run(&state, automation_id) {
            return Ok((in_flight, true));
        }
        let run = AutomationRun::queue_manual(
            AutomationRunId::mint(),
            automation.id,
            automation.execution.run_mode(),
            idempotency_key,
            now_ms,
        );
        state.runs.push(encode_run(&run));
        Ok((run, false))
    }

    /// Claims every window that is due, and arms the schedules that need it.
    ///
    /// The order is the whole single-flight argument, and it is one lock:
    ///
    /// 1. an enabled automation with no `nextRunAt` is armed — the state a row
    ///    from before the scheduler is in;
    /// 2. an automation whose window has arrived is claimed only when it has
    ///    nothing in flight, so a slow run delays the next one instead of
    ///    stacking on top of it;
    /// 3. the claim queues a run and immediately moves the schedule to its next
    ///    occurrence *after now*, which is what makes a restart harmless: the
    ///    window that was claimed is behind the automation's `nextRunAt` before
    ///    the snapshot is written, so a second sweep cannot see it as due
    ///    again. A window missed while the server was down is skipped, not
    ///    replayed, because "next" is computed from the claim instant.
    ///
    /// A trigger this build cannot evaluate (an expression or zone that does
    /// not resolve) is left alone with its `nextRunAt` intact and counted in
    /// the report; it is not a reason to delete an automation or to skip the
    /// rest of the sweep.
    pub fn sweep_due(&self, now_ms: u64) -> SweepReport {
        let mut report = SweepReport::default();
        let mut state = self.lock();
        // The stored `enabled` flag is the cheap filter; whether the row
        // decodes at all is what the loop reports.
        let mut positions: Vec<usize> = (0..state.automations.len())
            .filter(|position| state.automations[*position].enabled)
            .collect();
        // Oldest window first, then oldest row: the order the reference sweep
        // uses, so a bounded batch is the oldest work rather than an arbitrary
        // subset.
        positions.sort_by_key(|position| {
            let row = &state.automations[*position];
            (
                row.next_run_at.unwrap_or(u64::MAX),
                row.created_at,
                row.id.clone(),
            )
        });

        for position in positions {
            let row = state.automations[position].clone();
            let Ok(mut automation) = decode(&row) else {
                report.unreadable += 1;
                continue;
            };
            if !automation.enabled {
                continue;
            }
            let automation_id = automation.id.to_string();

            if automation.next_run_at.is_none() {
                if automation.arm_next_run(now_ms) {
                    state.automations[position] = encode(&automation);
                    report.armed += 1;
                } else if matches!(automation.trigger, AutomationTrigger::Schedule { .. }) {
                    report.unevaluable += 1;
                }
                continue;
            }
            if automation.next_run_at.is_some_and(|next| next > now_ms) {
                continue;
            }
            report.due += 1;
            if in_flight_run(&state, &automation_id).is_some() {
                report.in_flight += 1;
                continue;
            }
            // The next occurrence is computed from *now*, so a window that
            // passed while nobody was looking does not drag a queue of past
            // windows behind it.
            let next_run_at = match &automation.trigger {
                // A one-shot trigger is spent by its claim.
                AutomationTrigger::Once { .. } => None,
                AutomationTrigger::Schedule { cron, timezone } => {
                    let Ok(schedule) = Schedule::parse(cron, timezone) else {
                        // Nothing can be said about when this fires, so it is
                        // left exactly as it is rather than fired or disabled.
                        report.unevaluable += 1;
                        continue;
                    };
                    schedule.next_after(now_ms)
                }
            };
            let run = AutomationRun::queue_scheduled(
                AutomationRunId::mint(),
                automation.id.clone(),
                automation.execution.run_mode(),
                automation.next_run_at.unwrap_or(now_ms),
                now_ms,
            );
            automation.claim_scheduled_run(next_run_at, now_ms);
            if next_run_at.is_none()
                && matches!(automation.trigger, AutomationTrigger::Schedule { .. })
            {
                // An evaluable schedule with no remaining occurrence can never
                // fire again (`0 0 30 2 *`): disable it rather than leaving a
                // promise the scheduler cannot keep.
                automation.enabled = false;
                report.exhausted += 1;
            }
            state.runs.push(encode_run(&run));
            if !report.claimed_projects.contains(&automation.project_id) {
                report.claimed_projects.push(automation.project_id.clone());
            }
            state.automations[position] = encode(&automation);
            report.claimed += 1;
        }
        report
    }

    /// Moves a queued run to `running`, as the execution plane does when it
    /// claims it.
    pub fn start_run(
        &self,
        run_id: &AutomationRunId,
        now_ms: u64,
    ) -> Result<AutomationRun, AutomationError> {
        let mut state = self.lock();
        let position = state
            .runs
            .iter()
            .position(|row| row.id == run_id.to_string())
            .ok_or_else(|| AutomationError::NotFound(format!("run {run_id} is not known")))?;
        let mut run = decode_run(&state.runs[position]).ok_or_else(|| {
            AutomationError::Conflict(format!("run {run_id} has invalid stored data"))
        })?;
        run.start(now_ms)?;
        state.runs[position] = encode_run(&run);
        Ok(run)
    }

    /// Ends a run and applies the outcome to its automation.
    ///
    /// The two writes are one critical section on purpose: a failure that
    /// increments the counter without moving the retry instant, or a success
    /// that clears the counter without clearing the error, would be a state no
    /// single run produced. The run's `state` decides the retry — a failed
    /// *scheduled* run retries sooner and a third consecutive failure pauses
    /// the automation — see [`Automation::record_run_outcome`].
    pub fn close_run(
        &self,
        run_id: &AutomationRunId,
        outcome: &AutomationRunOutcome,
        now_ms: u64,
    ) -> Result<AutomationRun, AutomationError> {
        let mut state = self.lock();
        let position = state
            .runs
            .iter()
            .position(|row| row.id == run_id.to_string())
            .ok_or_else(|| AutomationError::NotFound(format!("run {run_id} is not known")))?;
        let mut run = decode_run(&state.runs[position]).ok_or_else(|| {
            AutomationError::Conflict(format!("run {run_id} has invalid stored data"))
        })?;
        let automation_id = run.automation_id.to_string();
        let trigger = run.trigger;
        run.finish(outcome, now_ms)?;
        let automation_position = state
            .automations
            .iter()
            .position(|row| row.id == automation_id);
        if let Some(automation_position) = automation_position {
            if let Ok(mut automation) = decode(&state.automations[automation_position]) {
                automation.record_run_outcome(outcome, trigger, now_ms);
                state.automations[automation_position] = encode(&automation);
            }
        }
        state.runs[position] = encode_run(&run);
        Ok(run)
    }

    /// Fails every run that was in flight when the process stopped.
    ///
    /// A pending run is *not* touched: it is durable work that has not started,
    /// and the sweep will simply not claim a second one while it waits. A
    /// `running` run is the one the execution plane can no longer speak for, so
    /// it is failed the same way the provider-run reconciler fails a dispatched
    /// run — the alternative is an automation that single-flight blocks
    /// forever behind a run nobody will ever finish. Returns the ids closed.
    pub fn fail_interrupted_runs(&self, now_ms: u64) -> Vec<AutomationRunId> {
        let running: Vec<AutomationRunId> = {
            let state = self.lock();
            state
                .runs
                .iter()
                .filter_map(decode_run)
                .filter(|run| run.state == AutomationRunState::Running)
                .map(|run| run.id)
                .collect()
        };
        let mut failed = Vec::new();
        for run_id in &running {
            let outcome = AutomationRunOutcome::Failed {
                error: "the server restarted while this run was in flight".to_owned(),
                thread_id: None,
                output: None,
                exit_code: None,
            };
            if self.close_run(run_id, &outcome, now_ms).is_ok() {
                failed.push(run_id.clone());
            }
        }
        failed
    }

    /// The runs still waiting for execution, oldest first.
    ///
    /// This is the executor's queue: a queued run is durable work, and the
    /// order is the order the queue was filled, so a burst of windows is
    /// dispatched in the order it arrived.
    pub fn pending_runs(&self, limit: usize) -> Vec<AutomationRun> {
        let state = self.lock();
        let mut runs: Vec<AutomationRun> = state
            .runs
            .iter()
            .filter_map(decode_run)
            .filter(|run| run.state == AutomationRunState::Pending)
            .collect();
        runs.sort_by(|left, right| {
            left.started_at
                .cmp(&right.started_at)
                .then_with(|| left.id.to_string().cmp(&right.id.to_string()))
        });
        runs.truncate(limit);
        runs
    }

    /// Whether an automation already has work in flight.
    pub fn has_in_flight(&self, automation_id: &str) -> bool {
        in_flight_run(&self.lock(), automation_id).is_some()
    }

    /// Records the thread and provider run an automation run became.
    ///
    /// The thread mark is written in the same critical section: a run that has
    /// a provider run but no mark would be a thread the server cannot tell was
    /// automation-produced, and the two facts are only ever true together.
    pub fn attach_run_dispatch(
        &self,
        run_id: &AutomationRunId,
        thread_id: &ThreadId,
        provider_run_id: &str,
        now_ms: u64,
    ) -> Result<AutomationRun, AutomationError> {
        let mut state = self.lock();
        let position = state
            .runs
            .iter()
            .position(|row| row.id == run_id.to_string())
            .ok_or_else(|| AutomationError::NotFound(format!("run {run_id} is not known")))?;
        let mut run = decode_run(&state.runs[position]).ok_or_else(|| {
            AutomationError::Conflict(format!("run {run_id} has invalid stored data"))
        })?;
        run.attach_dispatch(thread_id.clone(), provider_run_id.to_owned());
        state.runs[position] = encode_run(&run);
        let mark = encode_thread_mark(&AutomationThreadMark {
            thread_id: thread_id.clone(),
            automation_id: run.automation_id.clone(),
            run_id: run.id.clone(),
            created_at_ms: now_ms,
        });
        match state
            .thread_marks
            .iter_mut()
            .find(|row| row.thread_id == mark.thread_id)
        {
            Some(existing) => *existing = mark,
            None => state.thread_marks.push(mark),
        }
        Ok(run)
    }

    /// Records the host a script run was dispatched to, and moves it to
    /// `running`.
    ///
    /// The state change and the host are one write on purpose: a run that is
    /// `running` with no host is a run nothing can cancel and nothing can reap,
    /// which is the one state a script run must never reach.
    pub fn attach_script_dispatch(
        &self,
        run_id: &AutomationRunId,
        host_id: &loom_domain::HostId,
        now_ms: u64,
    ) -> Result<AutomationRun, AutomationError> {
        let mut state = self.lock();
        let position = state
            .runs
            .iter()
            .position(|row| row.id == run_id.to_string())
            .ok_or_else(|| AutomationError::NotFound(format!("run {run_id} is not known")))?;
        let mut run = decode_run(&state.runs[position]).ok_or_else(|| {
            AutomationError::Conflict(format!("run {run_id} has invalid stored data"))
        })?;
        run.attach_script_dispatch(host_id.clone());
        run.start(now_ms)?;
        state.runs[position] = encode_run(&run);
        Ok(run)
    }

    /// Records where the host wrote an automation's inline script.
    ///
    /// The path belongs to the machine that ran it, so it arrives with a
    /// report rather than being composed by the control plane. A run whose
    /// automation cannot be read is a no-op: nothing about the run's result
    /// depends on this.
    pub fn record_stored_script_path(&self, run_id: &AutomationRunId, path: &str, now_ms: u64) {
        let mut state = self.lock();
        let Some(automation_id) = state
            .runs
            .iter()
            .find(|row| row.id == run_id.to_string())
            .map(|row| row.automation_id.clone())
        else {
            return;
        };
        let Some(position) = state
            .automations
            .iter()
            .position(|row| row.id == automation_id)
        else {
            return;
        };
        let Ok(mut automation) = decode(&state.automations[position]) else {
            return;
        };
        match &mut automation.execution {
            AutomationExecution::Script(execution) => {
                if execution.stored_script_path.as_deref() == Some(path) {
                    return;
                }
                execution.stored_script_path = Some(path.to_owned());
            }
            AutomationExecution::Agent(_) => return,
        }
        automation.updated_at_ms = now_ms;
        state.automations[position] = encode(&automation);
    }

    /// Every script run that is executing right now.
    pub fn running_script_runs(&self) -> Vec<AutomationRun> {
        let state = self.lock();
        let mut runs: Vec<AutomationRun> = state
            .runs
            .iter()
            .filter_map(decode_run)
            .filter(|run| run.state == AutomationRunState::Running)
            .filter(|run| run.run_mode == AutomationRunMode::Script)
            .collect();
        runs.sort_by(|left, right| {
            left.started_at
                .cmp(&right.started_at)
                .then_with(|| left.id.to_string().cmp(&right.id.to_string()))
        });
        runs
    }

    /// Closes the automation run that became `provider_run_id`.
    ///
    /// The execution plane reports on provider runs; an automation run is a
    /// view of one, so this is where the view learns its outcome. `None` means
    /// no automation run claimed that provider run — an ordinary thread turn,
    /// or a run whose row was already closed.
    pub fn close_run_by_provider_run(
        &self,
        provider_run_id: &str,
        outcome: &AutomationRunOutcome,
        now_ms: u64,
    ) -> Option<AutomationRun> {
        let state = self.lock();
        let run_id = state
            .runs
            .iter()
            .filter(|row| row.provider_run_id.as_deref() == Some(provider_run_id))
            .filter_map(decode_run)
            .find(|run| run.is_in_flight())
            .map(|run| run.id)?;
        drop(state);
        self.close_run(&run_id, outcome, now_ms).ok()
    }

    /// One run by id.
    /// The project a run belongs to.
    ///
    /// A run row names its automation rather than its project, and the
    /// invalidation a settle publishes is project-scoped, so the lookup happens
    /// here — once, under one lock — instead of at each call site.
    pub fn project_of_run(&self, run_id: &AutomationRunId) -> Option<ProjectId> {
        let state = self.lock();
        let automation_id = state
            .runs
            .iter()
            .find(|row| row.id == run_id.to_string())
            .map(|row| row.automation_id.clone())?;
        let row = state
            .automations
            .iter()
            .find(|row| row.id == automation_id)?;
        decode(row).ok().map(|automation| automation.project_id)
    }

    pub fn run(&self, run_id: &AutomationRunId) -> Option<AutomationRun> {
        let state = self.lock();
        state
            .runs
            .iter()
            .find(|row| row.id == run_id.to_string())
            .and_then(decode_run)
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

impl std::fmt::Display for AutomationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(message) | Self::Conflict(message) => f.write_str(message),
            Self::Invalid { field, reason } => write!(f, "{field} {reason}"),
        }
    }
}

impl std::error::Error for AutomationError {}

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
    state.publish_automations_changed(&automation.project_id.to_string());
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
    state.publish_automations_changed(&automation.project_id.to_string());
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
    // Stop a script that is still running before its rows disappear: the
    // process belongs to the machine, and a deleted automation must not leave
    // one behind that nothing can name any more.
    let stopped = state.cancel_script_runs(
        &automation_id,
        "cancelled: the automation was deleted",
        loom_relay::now_ms(),
    );
    if !stopped.is_empty() {
        eprintln!(
            "loom-server: deleting automation {automation_id} stopped {} running script run(s)",
            stopped.len()
        );
    }
    if let Err(error) = state.automations.delete(&project_id, &automation_id) {
        return error_response(error);
    }
    if let Err(response) = persist(&state) {
        return response;
    }
    // The automation's rows are gone, so every client holding its list or its
    // history refetches.
    state.publish_automations_changed(&project_id);
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
    let (automation, cancelled) =
        match state
            .automations
            .set_enabled(&project_id, &automation_id, enabled, now)
        {
            Ok(outcome) => outcome,
            Err(error) => return error_response(error),
        };
    if !cancelled.is_empty() {
        eprintln!(
            "loom-server: pausing automation {automation_id} cancelled {} queued run(s)",
            cancelled.len()
        );
    }
    // A script that is already executing is a process on a known machine, so a
    // pause can stop it — unlike an agent run, whose provider protocol has no
    // cancel frame. The run is settled here and the host's report that follows
    // finds nothing in flight.
    if !enabled {
        let stopped =
            state.cancel_script_runs(&automation_id, "cancelled: the automation was paused", now);
        if !stopped.is_empty() {
            eprintln!(
                "loom-server: pausing automation {automation_id} stopped {} running script run(s)",
                stopped.len()
            );
        }
    }
    if let Err(response) = persist(&state) {
        return response;
    }
    // A pause or resume changes the automation *and* every run it cancelled,
    // so one frame per project covers both.
    state.publish_automations_changed(&project_id);
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
    let (run, deduped) = match state.automations.queue_manual_run(
        &project_id,
        &automation_id,
        request.idempotency_key,
        now,
    ) {
        Ok(outcome) => outcome,
        Err(error) => return error_response(error),
    };
    // A manual run is an execution intent, and the client that asked for it is
    // waiting: dispatch it now rather than at the next scheduled tick. The
    // executor is the same one the sweep uses, so a manual run and a due window
    // cannot take different paths.
    if !deduped {
        state.execute_pending_automation_runs(now);
    }
    if let Err(response) = persist(&state) {
        return response;
    }
    // A run that already existed is not a change: the client that sent the
    // duplicate key gets the run back, and everyone else has seen it.
    if !deduped {
        state.publish_automations_changed(&project_id);
    }
    // The response is the run as it stands after the dispatch attempt: a run
    // that reached a host is `running` with its thread, and one that could not
    // be dispatched is already `failed` with the reason.
    let run = state.automations.run(&run.id).unwrap_or(run);
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
    use crate::state::realtime_test_support::{drain, expect_project_invalidation, no_change};
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

    /* -------------------------------------------------------------- */
    /* Public invalidation                                             */
    /* -------------------------------------------------------------- */

    /// A server with its background loops off, and a subscription to what it
    /// tells public clients.
    fn served() -> (
        AppState,
        tokio::sync::broadcast::Receiver<crate::pump::PublicRealtimeEvent>,
    ) {
        let state = AppState::build(crate::state::AppConfig {
            reconcile_interval: std::time::Duration::ZERO,
            schedule_interval: std::time::Duration::ZERO,
            ..crate::state::AppConfig::default()
        })
        .expect("builds");
        let events = state.public_events.subscribe();
        (state, events)
    }

    #[tokio::test]
    async fn creating_updating_and_deleting_each_invalidate_their_project() {
        let (state, mut events) = served();
        let project = state.registry.personal_project_id().to_string();

        let response = create(
            State(state.clone()),
            Path(project.clone()),
            Json(new_automation("nightly")),
        )
        .await;
        assert_eq!(response.status(), StatusCode::CREATED);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("a body");
        let automation = serde_json::from_slice::<serde_json::Value>(&body)
            .expect("json")
            .get("id")
            .and_then(|id| id.as_str())
            .expect("an id")
            .to_owned();
        expect_project_invalidation(&mut events, &project).await;

        let response = update(
            State(state.clone()),
            Path((project.clone(), automation.clone())),
            Json(AutomationUpdate {
                name: Some("nightly review".into()),
                ..AutomationUpdate::default()
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        expect_project_invalidation(&mut events, &project).await;

        let response = delete(
            State(state.clone()),
            Path((project.clone(), automation.clone())),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        expect_project_invalidation(&mut events, &project).await;

        // A read never invalidates anything: only mutations do.
        let response = get(State(state.clone()), Path((project.clone(), automation))).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        no_change(&mut events).await;
        state.shutdown();
    }

    #[tokio::test]
    async fn pausing_and_resuming_invalidate_the_project() {
        let (state, mut events) = served();
        let project = state.registry.personal_project_id().to_string();
        let automation = state
            .automations
            .create(project_of(&state), new_automation("mornings"), now())
            .expect("creates");

        let response = pause(
            State(state.clone()),
            Path((project.clone(), automation.id.to_string())),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        expect_project_invalidation(&mut events, &project).await;

        let response = resume(
            State(state.clone()),
            Path((project.clone(), automation.id.to_string())),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        expect_project_invalidation(&mut events, &project).await;
        state.shutdown();
    }

    #[tokio::test]
    async fn a_manual_run_invalidates_the_project_once_and_a_duplicate_does_not() {
        let (state, mut events) = served();
        let project = state.registry.personal_project_id().to_string();
        let automation = state
            .automations
            .create(project_of(&state), new_automation("on demand"), now())
            .expect("creates");

        let response = run(
            State(state.clone()),
            Path((project.clone(), automation.id.to_string())),
            Some(Json(RunAutomationRequest {
                idempotency_key: Some("once".into()),
            })),
        )
        .await;
        assert_eq!(response.status(), StatusCode::CREATED);
        expect_project_invalidation(&mut events, &project).await;
        drain(&mut events).await;

        // The same key answers with the run it already created; nothing new
        // exists, so nothing new is announced.
        let response = run(
            State(state.clone()),
            Path((project.clone(), automation.id.to_string())),
            Some(Json(RunAutomationRequest {
                idempotency_key: Some("once".into()),
            })),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        no_change(&mut events).await;
        state.shutdown();
    }

    #[tokio::test]
    async fn a_scheduled_claim_invalidates_the_project_that_owns_it() {
        let (state, mut events) = served();
        let project = state.registry.personal_project_id().to_string();
        let automation = state
            .automations
            .create(project_of(&state), new_automation("scheduled"), now())
            .expect("creates");
        make_due(&state.automations, &automation.id, now() - 1_000);

        let report = state.sweep_automations(now());
        assert_eq!(report.claimed, 1, "{report:?}");
        assert_eq!(
            report.claimed_projects,
            vec![project_of(&state)],
            "{report:?}"
        );
        expect_project_invalidation(&mut events, &project).await;
        state.shutdown();
    }

    /// The stored project, as the routes take it.
    fn project_of(state: &AppState) -> ProjectId {
        state.registry.personal_project_id()
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
            .queue_manual_run(&project, &automation_id, None, now())
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
            .queue_manual_run(&project, &automation_id, Some("key-1".into()), now())
            .expect("starts");
        assert!(!deduped);
        let (second, deduped) = registry
            .queue_manual_run(&project, &automation_id, Some("key-1".into()), now() + 1)
            .expect("dedupes");
        assert!(deduped);
        assert_eq!(second.id, first.id);
        // A different key does not start a second run while one is in flight.
        let (third, deduped) = registry
            .queue_manual_run(&project, &automation_id, Some("key-2".into()), now() + 2)
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
            registry.queue_manual_run(&project, &automation_id, Some(key), now()),
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
                let mut run = AutomationRun::queue_manual(
                    AutomationRunId::mint(),
                    created.id.clone(),
                    AutomationRunMode::Agent,
                    None,
                    now() + index,
                );
                run.state = AutomationRunState::Succeeded;
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
            stored_script_path: None,
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

    /// Moves an automation's next window into the past: the state a server that
    /// was not running when the window arrived restores into.
    fn make_due(registry: &AutomationsRegistry, automation_id: &AutomationId, window: u64) {
        let mut state = registry.lock();
        let position = state
            .automations
            .iter()
            .position(|row| row.id == automation_id.to_string())
            .expect("the automation is stored");
        state.automations[position].next_run_at = Some(window);
    }

    /// The decoded automation, as a reader would see it.
    fn decoded(
        registry: &AutomationsRegistry,
        project: &ProjectId,
        id: &AutomationId,
    ) -> Automation {
        let row = registry
            .get(&project.to_string(), &id.to_string())
            .expect("the automation is stored");
        decode(&row).expect("the row decodes")
    }

    /// Every run of an automation, newest first.
    fn runs_of(
        registry: &AutomationsRegistry,
        project: &ProjectId,
        id: &AutomationId,
    ) -> Vec<AutomationRun> {
        registry
            .runs(&project.to_string(), &id.to_string(), 50, None)
            .expect("the run list reads")
            .0
    }

    fn succeeded() -> AutomationRunOutcome {
        AutomationRunOutcome::Succeeded {
            thread_id: None,
            output: None,
            exit_code: None,
        }
    }

    fn failed(error: &str) -> AutomationRunOutcome {
        AutomationRunOutcome::Failed {
            error: error.to_owned(),
            thread_id: None,
            output: None,
            exit_code: None,
        }
    }

    #[test]
    fn a_due_window_is_claimed_once_and_the_schedule_moves_past_now() {
        let registry = AutomationsRegistry::new();
        let project = project_id();
        let created = registry
            .create(project.clone(), new_automation("nightly"), now())
            .expect("creates");
        assert!(
            created.next_run_at.is_some_and(|next| next > now()),
            "an enabled schedule is armed at creation"
        );
        let window = now() - 60_000;
        make_due(&registry, &created.id, window);

        let report = registry.sweep_due(now());
        assert_eq!(report.due, 1);
        assert_eq!(report.claimed, 1);
        assert_eq!(report.in_flight, 0);
        assert!(report.changed());

        let automation = decoded(&registry, &project, &created.id);
        assert_eq!(automation.run_count, 1);
        assert_eq!(automation.last_run_at, Some(now()));
        assert_eq!(
            automation.last_run_status,
            Some(AutomationRunState::Pending)
        );
        assert!(
            automation.next_run_at.is_some_and(|next| next > now()),
            "the next window is in the future, not the one just claimed"
        );

        let runs = runs_of(&registry, &project, &created.id);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].trigger, AutomationRunTrigger::Schedule);
        assert_eq!(runs[0].scheduled_for, window);
        assert_eq!(runs[0].state, AutomationRunState::Pending);
        assert_eq!(
            runs[0].response().status,
            AutomationRunStatus::Running,
            "a queued run reads as in flight"
        );
    }

    #[test]
    fn a_second_sweep_does_not_claim_the_window_again() {
        let registry = AutomationsRegistry::new();
        let project = project_id();
        let created = registry
            .create(project.clone(), new_automation("nightly"), now())
            .expect("creates");
        make_due(&registry, &created.id, now() - 1);
        assert_eq!(registry.sweep_due(now()).claimed, 1);

        let second = registry.sweep_due(now() + 1);
        assert_eq!(second.due, 0, "the window is behind nextRunAt now");
        assert_eq!(second.claimed, 0);
        assert_eq!(runs_of(&registry, &project, &created.id).len(), 1);
    }

    #[test]
    fn a_run_in_flight_holds_the_next_window_back() {
        let registry = AutomationsRegistry::new();
        let project = project_id();
        let created = registry
            .create(project.clone(), new_automation("nightly"), now())
            .expect("creates");
        let project_key = project.to_string();
        let automation_key = created.id.to_string();
        registry
            .queue_manual_run(&project_key, &automation_key, None, now())
            .expect("queues");
        make_due(&registry, &created.id, now() - 1_000);

        let report = registry.sweep_due(now());
        assert_eq!(report.due, 1);
        assert_eq!(report.claimed, 0);
        assert_eq!(report.in_flight, 1);
        assert_eq!(
            decoded(&registry, &project, &created.id).next_run_at,
            Some(now() - 1_000),
            "the window waits for the run in flight instead of being dropped"
        );
        assert_eq!(runs_of(&registry, &project, &created.id).len(), 1);

        // Once the run is finished, the same window is claimable.
        let run_id = runs_of(&registry, &project, &created.id)[0].id.clone();
        registry
            .close_run(&run_id, &succeeded(), now() + 1)
            .expect("closes");
        assert_eq!(registry.sweep_due(now() + 2).claimed, 1);
        assert_eq!(runs_of(&registry, &project, &created.id).len(), 2);
    }

    #[test]
    fn a_scheduled_run_blocks_a_manual_one_through_the_same_single_flight() {
        let registry = AutomationsRegistry::new();
        let project = project_id();
        let created = registry
            .create(project.clone(), new_automation("nightly"), now())
            .expect("creates");
        make_due(&registry, &created.id, now() - 1);
        assert_eq!(registry.sweep_due(now()).claimed, 1);

        let (run, deduped) = registry
            .queue_manual_run(
                &project.to_string(),
                &created.id.to_string(),
                None,
                now() + 1,
            )
            .expect("queues");
        assert!(
            deduped,
            "the queued scheduled run is what the request resolves to"
        );
        assert_eq!(run.trigger, AutomationRunTrigger::Schedule);
        assert_eq!(runs_of(&registry, &project, &created.id).len(), 1);
    }

    #[test]
    fn a_once_trigger_is_spent_by_its_claim() {
        let registry = AutomationsRegistry::new();
        let project = project_id();
        let mut new = new_automation("one shot");
        new.trigger = AutomationTrigger::Once {
            run_at: now() + 60_000,
        };
        let created = registry
            .create(project.clone(), new, now())
            .expect("creates");
        make_due(&registry, &created.id, now() - 1);

        let report = registry.sweep_due(now());
        assert_eq!(report.claimed, 1);
        let automation = decoded(&registry, &project, &created.id);
        assert!(
            !automation.enabled,
            "a one-shot automation does not fire twice"
        );
        assert_eq!(automation.next_run_at, None);
        assert_eq!(registry.sweep_due(now() + 1).claimed, 0);
        assert_eq!(runs_of(&registry, &project, &created.id).len(), 1);
    }

    #[test]
    fn a_spent_schedule_stops_instead_of_claiming_windows_forever() {
        let registry = AutomationsRegistry::new();
        let project = project_id();
        let mut new = new_automation("never");
        // A date that never arrives — the claim can only happen once.
        new.trigger = AutomationTrigger::Schedule {
            cron: "0 0 30 2 *".into(),
            timezone: "UTC".into(),
        };
        let created = registry
            .create(project.clone(), new, now())
            .expect("creates");
        assert_eq!(
            created.next_run_at, None,
            "a schedule with no next occurrence is not armed"
        );
        make_due(&registry, &created.id, now() - 1);

        let report = registry.sweep_due(now());
        assert_eq!(report.claimed, 1);
        assert_eq!(report.exhausted, 1);
        let automation = decoded(&registry, &project, &created.id);
        assert!(!automation.enabled);
        assert_eq!(automation.next_run_at, None);
        assert_eq!(registry.sweep_due(now() + 1).claimed, 0);
    }

    #[test]
    fn a_schedule_this_build_cannot_evaluate_does_not_fire_blindly() {
        let registry = AutomationsRegistry::new();
        let project = project_id();
        let created = registry
            .create(project.clone(), new_automation("nightly"), now())
            .expect("creates");
        {
            let mut state = registry.lock();
            // A zone the database does not know: it parses as a string and is
            // refused by every attempt to evaluate it.
            state.automations[0].trigger = json!({ "triggerType": "schedule", "cron": "0 9 * * *", "timezone": "Mars/Olympus" });
            state.automations[0].next_run_at = Some(now() - 1);
        }

        let report = registry.sweep_due(now());
        assert_eq!(report.claimed, 0);
        assert_eq!(report.unevaluable, 1);
        assert_eq!(
            decoded(&registry, &project, &created.id).next_run_at,
            Some(now() - 1),
            "the row is left exactly as it was"
        );
        assert!(runs_of(&registry, &project, &created.id).is_empty());
    }

    #[test]
    fn pausing_cancels_queued_runs_and_resuming_does_not_replay_the_window() {
        let registry = AutomationsRegistry::new();
        let project = project_id();
        let created = registry
            .create(project.clone(), new_automation("nightly"), now())
            .expect("creates");
        let project_key = project.to_string();
        let automation_key = created.id.to_string();
        registry
            .queue_manual_run(&project_key, &automation_key, None, now())
            .expect("queues");
        make_due(&registry, &created.id, now() - 60_000);

        let (paused, cancelled) = registry
            .set_enabled(&project_key, &automation_key, false, now() + 1)
            .expect("pauses");
        assert!(!paused.enabled);
        assert_eq!(paused.next_run_at, None);
        assert_eq!(cancelled.len(), 1, "the queued run is abandoned");

        let runs = runs_of(&registry, &project, &created.id);
        assert_eq!(runs[0].state, AutomationRunState::Cancelled);
        assert_eq!(runs[0].response().status, AutomationRunStatus::Skipped);
        assert!(runs[0]
            .skip_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("paused")));

        // Nothing fires while it is paused, and the missed window is not
        // replayed on resume.
        assert_eq!(registry.sweep_due(now() + 2).claimed, 0);
        let (resumed, cancelled) = registry
            .set_enabled(&project_key, &automation_key, true, now() + 3)
            .expect("resumes");
        assert!(cancelled.is_empty());
        assert!(resumed.enabled);
        assert!(
            resumed.next_run_at.is_some_and(|next| next > now() + 3),
            "the schedule re-arms from now"
        );
    }

    #[test]
    fn the_run_state_machine_moves_pending_to_running_to_terminal() {
        let registry = AutomationsRegistry::new();
        let project = project_id();
        let created = registry
            .create(project.clone(), new_automation("nightly"), now())
            .expect("creates");
        let (run, _) = registry
            .queue_manual_run(&project.to_string(), &created.id.to_string(), None, now())
            .expect("queues");
        assert_eq!(run.state, AutomationRunState::Pending);

        let started = registry.start_run(&run.id, now() + 1).expect("starts");
        assert_eq!(started.state, AutomationRunState::Running);
        assert_eq!(started.started_at, now() + 1);

        let closed = registry
            .close_run(&run.id, &succeeded(), now() + 2)
            .expect("closes");
        assert_eq!(closed.state, AutomationRunState::Succeeded);
        assert_eq!(closed.finished_at, Some(now() + 2));

        // Terminal is terminal, and a run starts once.
        assert!(registry
            .close_run(&run.id, &succeeded(), now() + 3)
            .is_err());
        assert!(registry.start_run(&run.id, now() + 4).is_err());
    }

    #[test]
    fn a_failure_schedules_a_retry_and_the_third_one_pauses_the_automation() {
        let registry = AutomationsRegistry::new();
        let project = project_id();
        let created = registry
            .create(project.clone(), new_automation("nightly"), now())
            .expect("creates");

        let mut failure_at = now();
        for attempt in 1..=3u32 {
            make_due(&registry, &created.id, failure_at - 1_000);
            assert_eq!(
                registry.sweep_due(failure_at).claimed,
                1,
                "attempt {attempt}"
            );
            let run_id = runs_of(&registry, &project, &created.id)[0].id.clone();
            registry
                .close_run(&run_id, &failed("provider exploded"), failure_at + 1)
                .expect("closes");

            let automation = decoded(&registry, &project, &created.id);
            assert_eq!(automation.consecutive_failures, attempt);
            assert_eq!(automation.last_run_status, Some(AutomationRunState::Failed));
            if attempt < 3 {
                let expected =
                    failure_at + 1 + loom_domain::automation::automation_retry_delay_ms(attempt);
                assert_eq!(
                    automation.next_run_at,
                    Some(expected),
                    "attempt {attempt} retries sooner than the next window"
                );
                assert!(automation.enabled);
            } else {
                assert!(!automation.enabled, "three failures pause the automation");
                assert_eq!(automation.next_run_at, None);
                assert!(automation
                    .last_error
                    .as_deref()
                    .is_some_and(|error| error.contains("paused after 3 consecutive failures")));
            }
            failure_at += 10_000;
        }
    }

    #[test]
    fn a_manual_failure_does_not_schedule_a_retry() {
        let registry = AutomationsRegistry::new();
        let project = project_id();
        let created = registry
            .create(project.clone(), new_automation("nightly"), now())
            .expect("creates");
        let armed = created.next_run_at.expect("armed");
        let (run, _) = registry
            .queue_manual_run(&project.to_string(), &created.id.to_string(), None, now())
            .expect("queues");
        registry
            .close_run(&run.id, &failed("nobody asked for this twice"), now() + 1)
            .expect("closes");

        let automation = decoded(&registry, &project, &created.id);
        assert_eq!(automation.consecutive_failures, 1);
        assert_eq!(
            automation.next_run_at,
            Some(armed),
            "a manual failure leaves the schedule where it was"
        );
        assert_eq!(
            automation.last_error.as_deref(),
            Some("nobody asked for this twice")
        );
    }

    #[test]
    fn a_success_clears_the_failure_state_and_records_the_thread() {
        let registry = AutomationsRegistry::new();
        let project = project_id();
        let created = registry
            .create(project.clone(), new_automation("nightly"), now())
            .expect("creates");
        let (run, _) = registry
            .queue_manual_run(&project.to_string(), &created.id.to_string(), None, now())
            .expect("queues");
        registry
            .close_run(&run.id, &failed("first try"), now() + 1)
            .expect("closes");
        let thread_id = ThreadId::mint();
        let (second, _) = registry
            .queue_manual_run(
                &project.to_string(),
                &created.id.to_string(),
                Some("second".into()),
                now() + 2,
            )
            .expect("queues");
        let outcome = AutomationRunOutcome::Succeeded {
            thread_id: Some(thread_id.clone()),
            output: Some("done".into()),
            exit_code: Some(0),
        };
        let closed = registry
            .close_run(&second.id, &outcome, now() + 3)
            .expect("closes");
        assert_eq!(closed.thread_id, Some(thread_id.clone()));
        assert_eq!(closed.output.as_deref(), Some("done"));
        assert_eq!(closed.exit_code, Some(0));

        let automation = decoded(&registry, &project, &created.id);
        assert_eq!(automation.consecutive_failures, 0);
        assert_eq!(automation.last_error, None);
        assert_eq!(
            automation.last_run_status,
            Some(AutomationRunState::Succeeded)
        );
        assert_eq!(automation.last_run_thread_id, Some(thread_id));
    }

    #[test]
    fn a_restart_fails_runs_in_flight_and_keeps_queued_ones() {
        let registry = AutomationsRegistry::new();
        let project = project_id();
        let created = registry
            .create(project.clone(), new_automation("nightly"), now())
            .expect("creates");
        let running_id = {
            let mut state = registry.lock();
            let mut running = AutomationRun::queue_manual(
                AutomationRunId::mint(),
                created.id.clone(),
                AutomationRunMode::Agent,
                None,
                now(),
            );
            running.start(now()).expect("starts");
            let queued = AutomationRun::queue_manual(
                AutomationRunId::mint(),
                created.id.clone(),
                AutomationRunMode::Agent,
                Some("queued".into()),
                now(),
            );
            state.runs.push(encode_run(&running));
            state.runs.push(encode_run(&queued));
            running.id
        };

        let failed = registry.fail_interrupted_runs(now() + 1);
        assert_eq!(failed, vec![running_id.clone()]);

        let runs = runs_of(&registry, &project, &created.id);
        assert_eq!(
            runs.iter()
                .filter(|run| run.state == AutomationRunState::Running)
                .count(),
            0
        );
        assert_eq!(
            runs.iter()
                .filter(|run| run.state == AutomationRunState::Pending)
                .count(),
            1,
            "a queued run is durable work and survives the restart"
        );
        let interrupted = runs
            .iter()
            .find(|run| run.id == running_id)
            .expect("the interrupted run is there");
        assert_eq!(interrupted.state, AutomationRunState::Failed);
        assert!(interrupted
            .error
            .as_deref()
            .is_some_and(|error| error.contains("restarted")));
        // The failure is recorded on the automation, exactly as any other one.
        assert_eq!(
            decoded(&registry, &project, &created.id).consecutive_failures,
            1
        );
    }

    #[test]
    fn a_payload_from_before_the_scheduler_migrates_and_is_armed_by_the_sweep() {
        let registry = AutomationsRegistry::new();
        let project = project_id();
        let created = registry
            .create(project.clone(), new_automation("nightly"), now())
            .expect("creates");
        let mut payload = registry.export();
        payload.version = 1;
        payload.automations[0].next_run_at = None;
        // Version 1 could not claim a run, so what it stored as `running` was
        // a queue entry nothing had picked up.
        payload.runs.push(StoredAutomationRun {
            id: AutomationRunId::mint().to_string(),
            automation_id: created.id.to_string(),
            run_mode: "agent".into(),
            status: "running".into(),
            trigger: "manual".into(),
            scheduled_for: now(),
            started_at: now(),
            ..StoredAutomationRun::default()
        });
        registry.restore(payload);

        assert_eq!(registry.export().version, AUTOMATIONS_VERSION);
        let runs = runs_of(&registry, &project, &created.id);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].state, AutomationRunState::Pending);

        let report = registry.sweep_due(now());
        assert_eq!(report.armed, 1, "the schedule is armed by the sweep");
        assert!(decoded(&registry, &project, &created.id)
            .next_run_at
            .is_some_and(|next| next > now()));
    }

    #[test]
    fn a_row_this_build_cannot_read_does_not_stop_the_sweep() {
        let registry = AutomationsRegistry::new();
        let project = project_id();
        let created = registry
            .create(project.clone(), new_automation("nightly"), now())
            .expect("creates");
        make_due(&registry, &created.id, now() - 1);
        {
            let mut state = registry.lock();
            state.automations.push(StoredAutomation {
                enabled: true,
                ..StoredAutomation::default()
            });
        }

        let report = registry.sweep_due(now());
        assert_eq!(report.unreadable, 1);
        assert_eq!(report.claimed, 1, "the readable row is still claimed");
    }

    #[test]
    fn the_manual_run_request_is_idempotent_by_key() {
        let registry = AutomationsRegistry::new();
        let project = project_id();
        let created = registry
            .create(project.clone(), new_automation("nightly"), now())
            .expect("creates");
        let project_key = project.to_string();
        let automation_key = created.id.to_string();
        let (first, deduped) = registry
            .queue_manual_run(&project_key, &automation_key, Some("key-1".into()), now())
            .expect("queues");
        assert!(!deduped);
        registry
            .close_run(&first.id, &succeeded(), now() + 1)
            .expect("closes");
        let (second, deduped) = registry
            .queue_manual_run(
                &project_key,
                &automation_key,
                Some("key-1".into()),
                now() + 2,
            )
            .expect("dedupes");
        assert!(deduped);
        assert_eq!(second.id, first.id);
        assert_eq!(runs_of(&registry, &project, &created.id).len(), 1);
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
