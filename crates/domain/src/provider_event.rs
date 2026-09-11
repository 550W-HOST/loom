//! The provider event model: bb's `ThreadEvent` union, as loom types it.
//!
//! loom reuses bb's projection layer to turn an event stream into a timeline.
//! That layer dispatches on `event.type` and reads camelCase fields, so the
//! *wire representation is the contract*: renaming a field or a discriminant
//! silently breaks the timeline. This module is the Rust side of that
//! contract, mirroring `contracts/bb/thread-event.json` (generated from bb's
//! `packages/domain/src/provider-event.ts`).
//!
//! # Shape
//!
//! A [`ThreadEvent`] is a flattened object, exactly as bb stores and serves it:
//!
//! ```json
//! {
//!   "type": "item/agentMessage/delta",
//!   "threadId": "thr_...",
//!   "scope": { "kind": "turn", "turnId": "run_..." },
//!   "providerThreadId": "thr_...",
//!   "itemId": "assistant-1",
//!   "delta": "hello"
//! }
//! ```
//!
//! `threadId` and `scope` are common to every event and live on the struct;
//! everything else is the [`ProviderEvent`] body, internally tagged by `type`.
//!
//! # Scope
//!
//! `scope` is `{"kind":"thread"}` for thread-level facts (identity, goal,
//! background tasks) or `{"kind":"turn","turnId":…}` for turn chronology. A
//! provider run maps onto one turn: loom uses the run's id as its `turnId`, so
//! a reconnecting client groups a turn the same way bb does.
//!
//! # What is modelled
//!
//! All **35 provider event types** bb declares in
//! `providerEventTypeValues` have a [`ProviderEvent`] variant: nothing a
//! provider can emit is silently dropped. The 13 `system/*` and `client/*`
//! types are enumerated in [`ThreadEventType`] but have no body variant: they
//! are authored by a server or a client, never by a provider, so loom decodes
//! one as an explicit [`EventModelError`] rather than inventing a fallback.
//! `docs/event-model.md` records the full per-type decision.
//!
//! [`EventModelError`]: crate::ProviderEventError

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::id::{RunId, ThreadId};

/// Which room a thread event belongs to.
///
/// Mirrors bb's `threadEventScopeSchema`: either the thread as a whole or one
/// turn of it. The scope is what a client uses to place an event in turn
/// chronology, so it is carried on every event rather than inferred.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ThreadEventScope {
    /// A thread-level fact: not part of any one turn's transcript.
    Thread,
    /// A fact about one turn.
    Turn {
        /// The turn's id. loom uses the run id, so a turn survives a restart.
        #[serde(rename = "turnId")]
        turn_id: String,
    },
}

impl ThreadEventScope {
    /// The turn scope for a run.
    pub fn turn(run_id: &RunId) -> Self {
        ThreadEventScope::Turn {
            turn_id: run_id.to_string(),
        }
    }

    /// The turn id, when this is a turn scope.
    pub fn turn_id(&self) -> Option<&str> {
        match self {
            ThreadEventScope::Thread => None,
            ThreadEventScope::Turn { turn_id } => Some(turn_id),
        }
    }
}

/// How a turn ended, as `turn/completed.status`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnStatus {
    /// The turn ran to completion.
    Completed,
    /// The turn failed.
    Failed,
    /// The turn was interrupted (a cancelled or stale run).
    Interrupted,
}

/// The `error` object on `turn/completed`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnError {
    /// The message, verbatim.
    pub message: String,
}

/// Goal lifecycle status (`thread/goal/updated`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum GoalStatus {
    /// The goal is being pursued.
    Active,
    /// The goal is paused.
    Paused,
    /// The token budget is exhausted.
    BudgetLimited,
    /// The goal completed.
    Complete,
}

/// A single plan step (`turn/plan/updated`, `planSteps` items).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanStepStatus {
    /// Not started.
    Pending,
    /// In progress.
    Active,
    /// Done.
    Completed,
    /// Failed.
    Failed,
}

/// One step of a plan.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanStep {
    /// The step text.
    pub step: String,
    /// Its status, when the provider reports one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<PlanStepStatus>,
}

/// Item lifecycle status (`item.status`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemStatus {
    /// Started, not finished.
    Pending,
    /// Finished successfully.
    Completed,
    /// Finished with a failure.
    Failed,
    /// Interrupted.
    Interrupted,
}

/// Approval state an item may be waiting on.
///
/// Note the difference from an absent value: the contract types this as
/// `enum | null` and *requires* the key, so `None` must serialize as `null`,
/// not be omitted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalStatus {
    /// Waiting for a human decision.
    WaitingForApproval,
    /// A human denied it.
    Denied,
}

/// A file change inside a `fileChange` item.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileChange {
    /// The path touched.
    pub path: String,
    /// What happened to it.
    pub kind: FileChangeKind,
    /// The destination of a move, when any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub move_path: Option<String>,
    /// A unified diff, when the provider supplies one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff: Option<String>,
}

/// The kind of a [`FileChange`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileChangeKind {
    /// A file was added.
    Add,
    /// A file was removed.
    Delete,
    /// A file was modified.
    Update,
}

/// One user-content block on a `userMessage` item.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum UserContent {
    /// Plain text.
    Text {
        /// The text.
        text: String,
    },
    /// A remote image.
    Image {
        /// The image URL.
        url: String,
    },
    /// A local image file.
    LocalImage {
        /// The path.
        path: String,
    },
    /// A local file attachment.
    LocalFile {
        /// The path.
        path: String,
    },
}

/// A search item's mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchMode {
    /// Search file contents.
    Content,
    /// Find paths.
    Path,
    /// List a directory.
    List,
}

/// The optional presentation override on an item.
///
/// loom does not author these; the type exists so a provider-supplied one round
/// trips instead of being rejected as an unknown field.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ItemPresentation {
    /// The full label.
    pub label: PresentationLabel,
    /// The icon.
    pub icon: PresentationIcon,
    /// A short title.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// A longer detail line.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// A badge shown next to the item.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub badge: Option<PresentationBadge>,
    /// A tint.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tint: Option<PresentationTint>,
    /// Whether to keep the item out of the transcript.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suppress: Option<bool>,
}

/// The `label` sub-object of [`ItemPresentation`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresentationLabel {
    /// The pending label.
    pub pending: String,
    /// The completed label.
    pub completed: String,
}

/// The `icon` sub-object of [`ItemPresentation`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresentationIcon {
    /// The glyph.
    pub glyph: String,
}

/// The `badge` sub-object of [`ItemPresentation`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresentationBadge {
    /// The glyph.
    pub glyph: String,
    /// The label.
    pub label: String,
    /// A short hint.
    pub hint: String,
    /// The tone.
    pub tone: PresentationTone,
}

/// The `tone` of a [`PresentationBadge`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PresentationTone {
    /// Neutral.
    Neutral,
    /// Destructive.
    Destructive,
}

/// The `tint` sub-object of [`ItemPresentation`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresentationTint {
    /// The light-mode colour.
    pub light: String,
    /// The dark-mode colour.
    pub dark: String,
}

/// Token accounting for one call.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenUsageBreakdown {
    /// Total tokens.
    pub total_tokens: u64,
    /// Input tokens.
    pub input_tokens: u64,
    /// Cached input tokens.
    pub cached_input_tokens: u64,
    /// Output tokens.
    pub output_tokens: u64,
    /// Reasoning output tokens.
    pub reasoning_output_tokens: u64,
}

/// Thread-level token usage: the last call, the running total and the model's
/// context window.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadTokenUsage {
    /// The running total for the thread.
    pub total: TokenUsageBreakdown,
    /// The most recent call.
    pub last: TokenUsageBreakdown,
    /// The model's context window, when known.
    pub model_context_window: Option<u64>,
}

/// Current context-window occupancy.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextWindowUsage {
    /// Tokens in context, when known.
    pub used_tokens: Option<u64>,
    /// The model's context window, when known.
    pub model_context_window: Option<u64>,
    /// Whether `used_tokens` is estimated rather than reported.
    pub estimated: bool,
}

/// A provider error category.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderErrorCategory {
    /// Steering was attempted against a settled turn.
    ActiveTurnNotSteerable,
    /// The provider rejected the request shape.
    BadRequest,
    /// The provider connection failed.
    ConnectionFailed,
    /// The context window is exhausted.
    ContextWindowExceeded,
    /// A billing problem.
    Billing,
    /// A budget was exceeded.
    BudgetExceeded,
    /// An internal provider error.
    Internal,
    /// The output token cap was hit.
    MaxOutputTokens,
    /// The turn cap was hit.
    MaxTurns,
    /// The provider is overloaded.
    Overloaded,
    /// A policy refusal.
    Policy,
    /// Rate limited.
    RateLimit,
    /// A sandbox failure.
    Sandbox,
    /// The stream disconnected mid-turn.
    StreamDisconnected,
    /// Structured-output retries were exhausted.
    StructuredOutputRetries,
    /// Rolling the thread back failed.
    ThreadRollbackFailed,
    /// Too many failed attempts.
    TooManyFailedAttempts,
    /// The credentials were rejected.
    Unauthorized,
    /// Anything the provider did not classify.
    Unknown,
}

/// A structured provider-error classification.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderErrorInfo {
    /// The category.
    pub category: ProviderErrorCategory,
    /// The provider's own code, when any.
    pub provider_code: Option<String>,
    /// The HTTP status, when any.
    pub http_status_code: Option<u64>,
}

/// A `provider/warning` category.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProviderWarningCategory {
    /// A deprecated option was used.
    #[serde(rename = "deprecation")]
    Deprecation,
    /// A configuration warning.
    #[serde(rename = "config")]
    Config,
    /// A general warning.
    #[serde(rename = "general")]
    General,
    /// Compaction was skipped.
    #[serde(rename = "compaction-skipped")]
    CompactionSkipped,
}

/// Why a provider fell back to another model.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelFallbackReason {
    /// The original model refused.
    Refusal,
    /// The provider moved the request.
    Provider,
}

/// The raw provider frame carried by `provider/unhandled`.
///
/// This is a *diagnostic* record of a frame loom does not model. It is not a
/// catch-all event: loom's own bridge reports an unmapped frame only when it
/// genuinely cannot classify it, and the raw payload is preserved verbatim so
/// an operator can see why.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProviderRawEvent {
    /// Always `"2.0"`.
    pub jsonrpc: String,
    /// The request id, when any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,
    /// The method.
    pub method: String,
    /// The params, when any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

/// A `provider.env-resolved` entry.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EnvResolvedEntry {
    /// The variable name.
    pub name: String,
    /// Where its value came from.
    pub source: EnvResolvedSource,
    /// The value, or a masked marker.
    pub value: EnvResolvedValue,
    /// Why it resolved this way.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// The origin of a resolved environment variable.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum EnvResolvedSource {
    /// The process's own shell environment.
    Shell,
    /// A plugin supplied it.
    Plugin {
        /// The plugin id.
        plugin: String,
    },
}

/// A resolved value: either the text or a marker that it is masked.
///
/// Untagged because the contract's union is `string | { masked: true }`: a
/// JSON string is the value and a JSON object is the mask marker.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum EnvResolvedValue {
    /// It was masked.
    Masked {
        /// Always `true`.
        masked: bool,
    },
    /// The literal value.
    Value(String),
}

/// One item in the `item/started` / `item/completed` payload.
///
/// This is the vocabulary the projection's timeline rows are built from, so
/// every kind bb declares has a variant here. Optional fields are omitted when
/// absent; fields the contract types as nullable are serialized as `null`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ThreadEventItem {
    /// A user message.
    UserMessage {
        /// The item id.
        id: String,
        /// The content blocks.
        content: Vec<UserContent>,
        /// The client request that produced it, when any.
        #[serde(skip_serializing_if = "Option::is_none")]
        client_request_id: Option<String>,
        /// The tool call this message belongs to, when any.
        #[serde(skip_serializing_if = "Option::is_none")]
        parent_tool_call_id: Option<String>,
    },
    /// The assistant's visible answer.
    AgentMessage {
        /// The item id.
        id: String,
        /// The answer text.
        text: String,
        /// A presentation override.
        #[serde(skip_serializing_if = "Option::is_none")]
        presentation: Option<ItemPresentation>,
        /// The tool call this message belongs to, when any.
        #[serde(skip_serializing_if = "Option::is_none")]
        parent_tool_call_id: Option<String>,
    },
    /// A shell command execution.
    CommandExecution {
        /// The item id.
        id: String,
        /// The command line.
        command: String,
        /// The working directory.
        cwd: String,
        /// Its status.
        status: ItemStatus,
        /// Its approval state. Required, nullable.
        approval_status: Option<ApprovalStatus>,
        /// The accumulated output.
        #[serde(skip_serializing_if = "Option::is_none")]
        aggregated_output: Option<String>,
        /// The process exit code.
        #[serde(skip_serializing_if = "Option::is_none")]
        exit_code: Option<i64>,
        /// How long it ran.
        #[serde(skip_serializing_if = "Option::is_none")]
        duration_ms: Option<f64>,
        /// A presentation override.
        #[serde(skip_serializing_if = "Option::is_none")]
        presentation: Option<ItemPresentation>,
        /// The tool call this item belongs to, when any.
        #[serde(skip_serializing_if = "Option::is_none")]
        parent_tool_call_id: Option<String>,
    },
    /// A set of file changes.
    FileChange {
        /// The item id.
        id: String,
        /// The changes.
        changes: Vec<FileChange>,
        /// Its status.
        status: ItemStatus,
        /// Its approval state. Required, nullable.
        approval_status: Option<ApprovalStatus>,
        /// A presentation override.
        #[serde(skip_serializing_if = "Option::is_none")]
        presentation: Option<ItemPresentation>,
        /// The tool call this item belongs to, when any.
        #[serde(skip_serializing_if = "Option::is_none")]
        parent_tool_call_id: Option<String>,
    },
    /// A web search.
    WebSearch {
        /// The item id.
        id: String,
        /// The queries issued.
        queries: Vec<String>,
        /// The result text. Required, nullable.
        result_text: Option<String>,
        /// A presentation override.
        #[serde(skip_serializing_if = "Option::is_none")]
        presentation: Option<ItemPresentation>,
        /// The tool call this item belongs to, when any.
        #[serde(skip_serializing_if = "Option::is_none")]
        parent_tool_call_id: Option<String>,
    },
    /// A web fetch.
    WebFetch {
        /// The item id.
        id: String,
        /// The fetched URL.
        url: String,
        /// The extraction prompt. Required, nullable.
        prompt: Option<String>,
        /// The extraction pattern. Required, nullable.
        pattern: Option<String>,
        /// The result text. Required, nullable.
        result_text: Option<String>,
        /// A presentation override.
        #[serde(skip_serializing_if = "Option::is_none")]
        presentation: Option<ItemPresentation>,
        /// The tool call this item belongs to, when any.
        #[serde(skip_serializing_if = "Option::is_none")]
        parent_tool_call_id: Option<String>,
    },
    /// An image the model looked at.
    ImageView {
        /// The item id.
        id: String,
        /// The image path.
        path: String,
        /// A presentation override.
        #[serde(skip_serializing_if = "Option::is_none")]
        presentation: Option<ItemPresentation>,
        /// The tool call this item belongs to, when any.
        #[serde(skip_serializing_if = "Option::is_none")]
        parent_tool_call_id: Option<String>,
    },
    /// A generated image.
    ImageGeneration {
        /// The item id.
        id: String,
        /// Its status.
        status: ItemStatus,
        /// The generation prompt. Required, nullable.
        prompt: Option<String>,
        /// Where it was written. Required, nullable.
        path: Option<String>,
        /// An inline result.
        #[serde(skip_serializing_if = "Option::is_none")]
        result: Option<String>,
        /// The error. Required, nullable.
        error: Option<String>,
        /// Whether the background is transparent.
        transparent_background: bool,
        /// A presentation override.
        #[serde(skip_serializing_if = "Option::is_none")]
        presentation: Option<ItemPresentation>,
        /// The tool call this item belongs to, when any.
        #[serde(skip_serializing_if = "Option::is_none")]
        parent_tool_call_id: Option<String>,
    },
    /// A file read.
    FileRead {
        /// The item id.
        id: String,
        /// The path read.
        path: String,
        /// The command used.
        #[serde(skip_serializing_if = "Option::is_none")]
        cmd: Option<String>,
        /// Its status.
        status: ItemStatus,
        /// A presentation override.
        #[serde(skip_serializing_if = "Option::is_none")]
        presentation: Option<ItemPresentation>,
        /// The tool call this item belongs to, when any.
        #[serde(skip_serializing_if = "Option::is_none")]
        parent_tool_call_id: Option<String>,
    },
    /// A code search.
    Search {
        /// The item id.
        id: String,
        /// The search mode.
        mode: SearchMode,
        /// The query.
        query: String,
        /// The path searched.
        #[serde(skip_serializing_if = "Option::is_none")]
        path: Option<String>,
        /// The command used.
        #[serde(skip_serializing_if = "Option::is_none")]
        cmd: Option<String>,
        /// Its status.
        status: ItemStatus,
        /// A presentation override.
        #[serde(skip_serializing_if = "Option::is_none")]
        presentation: Option<ItemPresentation>,
        /// The tool call this item belongs to, when any.
        #[serde(skip_serializing_if = "Option::is_none")]
        parent_tool_call_id: Option<String>,
    },
    /// A generic tool call.
    ToolCall {
        /// The item id.
        id: String,
        /// The MCP server, when the tool came from one.
        #[serde(skip_serializing_if = "Option::is_none")]
        server: Option<String>,
        /// The tool name.
        tool: String,
        /// The arguments.
        #[serde(skip_serializing_if = "Option::is_none")]
        arguments: Option<BTreeMap<String, Value>>,
        /// Its status.
        status: ItemStatus,
        /// The raw result.
        #[serde(skip_serializing_if = "Option::is_none")]
        result: Option<Value>,
        /// The error text.
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        /// How long it ran.
        #[serde(skip_serializing_if = "Option::is_none")]
        duration_ms: Option<f64>,
        /// A presentation override.
        #[serde(skip_serializing_if = "Option::is_none")]
        presentation: Option<ItemPresentation>,
        /// The tool call this item belongs to, when any.
        #[serde(skip_serializing_if = "Option::is_none")]
        parent_tool_call_id: Option<String>,
    },
    /// The model's reasoning.
    Reasoning {
        /// The item id.
        id: String,
        /// Summary lines.
        summary: Vec<String>,
        /// Full reasoning text blocks.
        content: Vec<String>,
        /// A presentation override.
        #[serde(skip_serializing_if = "Option::is_none")]
        presentation: Option<ItemPresentation>,
        /// The tool call this item belongs to, when any.
        #[serde(skip_serializing_if = "Option::is_none")]
        parent_tool_call_id: Option<String>,
    },
    /// A free-form plan.
    Plan {
        /// The item id.
        id: String,
        /// The plan text.
        text: String,
        /// A presentation override.
        #[serde(skip_serializing_if = "Option::is_none")]
        presentation: Option<ItemPresentation>,
        /// The tool call this item belongs to, when any.
        #[serde(skip_serializing_if = "Option::is_none")]
        parent_tool_call_id: Option<String>,
    },
    /// A structured plan.
    PlanSteps {
        /// The item id.
        id: String,
        /// The steps.
        steps: Vec<PlanStep>,
        /// Why the plan changed.
        #[serde(skip_serializing_if = "Option::is_none")]
        explanation: Option<String>,
        /// Its status.
        status: ItemStatus,
        /// A presentation override.
        #[serde(skip_serializing_if = "Option::is_none")]
        presentation: Option<ItemPresentation>,
        /// The tool call this item belongs to, when any.
        #[serde(skip_serializing_if = "Option::is_none")]
        parent_tool_call_id: Option<String>,
    },
    /// A context compaction boundary.
    ContextCompaction {
        /// The item id.
        id: String,
        /// A presentation override.
        #[serde(skip_serializing_if = "Option::is_none")]
        presentation: Option<ItemPresentation>,
        /// The tool call this item belongs to, when any.
        #[serde(skip_serializing_if = "Option::is_none")]
        parent_tool_call_id: Option<String>,
    },
    /// A background task.
    BackgroundTask {
        /// The item id.
        id: String,
        /// The task family, when any.
        #[serde(skip_serializing_if = "Option::is_none")]
        family_id: Option<String>,
        /// The task type.
        task_type: String,
        /// A description.
        description: String,
        /// Its transcript status.
        status: ItemStatus,
        /// Its task status.
        task_status: String,
        /// Whether to keep it out of the transcript.
        skip_transcript: bool,
        /// The workflow name.
        #[serde(skip_serializing_if = "Option::is_none")]
        workflow_name: Option<String>,
        /// A workflow snapshot.
        #[serde(skip_serializing_if = "Option::is_none")]
        workflow: Option<Value>,
        /// Its usage snapshot.
        #[serde(skip_serializing_if = "Option::is_none")]
        usage: Option<Value>,
        /// A summary.
        #[serde(skip_serializing_if = "Option::is_none")]
        summary: Option<String>,
        /// The error.
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        /// The output file.
        #[serde(skip_serializing_if = "Option::is_none")]
        output_file: Option<String>,
        /// A presentation override.
        #[serde(skip_serializing_if = "Option::is_none")]
        presentation: Option<ItemPresentation>,
        /// The tool call this item belongs to, when any.
        #[serde(skip_serializing_if = "Option::is_none")]
        parent_tool_call_id: Option<String>,
    },
    /// A delegated child agent.
    Delegation {
        /// The item id.
        id: String,
        /// The child's reference.
        child_ref: String,
        /// A label.
        label: String,
        /// Its status.
        status: ItemStatus,
        /// Whether it runs in the background.
        background: bool,
        /// A summary.
        #[serde(skip_serializing_if = "Option::is_none")]
        summary: Option<String>,
        /// A presentation override.
        #[serde(skip_serializing_if = "Option::is_none")]
        presentation: Option<ItemPresentation>,
        /// The tool call this item belongs to, when any.
        #[serde(skip_serializing_if = "Option::is_none")]
        parent_tool_call_id: Option<String>,
    },
    /// A provider extension state item.
    Extension {
        /// The item id.
        id: String,
        /// The extension kind.
        kind: String,
        /// The extension payload.
        payload: Value,
        /// Its status.
        status: ItemStatus,
        /// The extension's presentation.
        presentation: ItemPresentation,
        /// The tool call this item belongs to, when any.
        #[serde(skip_serializing_if = "Option::is_none")]
        parent_tool_call_id: Option<String>,
    },
}

impl ThreadEventItem {
    /// The item's stable `type` tag, matching the serialized form.
    pub fn kind(&self) -> &'static str {
        match self {
            ThreadEventItem::UserMessage { .. } => "userMessage",
            ThreadEventItem::AgentMessage { .. } => "agentMessage",
            ThreadEventItem::CommandExecution { .. } => "commandExecution",
            ThreadEventItem::FileChange { .. } => "fileChange",
            ThreadEventItem::WebSearch { .. } => "webSearch",
            ThreadEventItem::WebFetch { .. } => "webFetch",
            ThreadEventItem::ImageView { .. } => "imageView",
            ThreadEventItem::ImageGeneration { .. } => "imageGeneration",
            ThreadEventItem::FileRead { .. } => "fileRead",
            ThreadEventItem::Search { .. } => "search",
            ThreadEventItem::ToolCall { .. } => "toolCall",
            ThreadEventItem::Reasoning { .. } => "reasoning",
            ThreadEventItem::Plan { .. } => "plan",
            ThreadEventItem::PlanSteps { .. } => "planSteps",
            ThreadEventItem::ContextCompaction { .. } => "contextCompaction",
            ThreadEventItem::BackgroundTask { .. } => "backgroundTask",
            ThreadEventItem::Delegation { .. } => "delegation",
            ThreadEventItem::Extension { .. } => "extension",
        }
    }

    /// The item id.
    pub fn id(&self) -> &str {
        match self {
            ThreadEventItem::UserMessage { id, .. }
            | ThreadEventItem::AgentMessage { id, .. }
            | ThreadEventItem::CommandExecution { id, .. }
            | ThreadEventItem::FileChange { id, .. }
            | ThreadEventItem::WebSearch { id, .. }
            | ThreadEventItem::WebFetch { id, .. }
            | ThreadEventItem::ImageView { id, .. }
            | ThreadEventItem::ImageGeneration { id, .. }
            | ThreadEventItem::FileRead { id, .. }
            | ThreadEventItem::Search { id, .. }
            | ThreadEventItem::ToolCall { id, .. }
            | ThreadEventItem::Reasoning { id, .. }
            | ThreadEventItem::Plan { id, .. }
            | ThreadEventItem::PlanSteps { id, .. }
            | ThreadEventItem::ContextCompaction { id, .. }
            | ThreadEventItem::BackgroundTask { id, .. }
            | ThreadEventItem::Delegation { id, .. }
            | ThreadEventItem::Extension { id, .. } => id,
        }
    }
}

/// The body of a [`ThreadEvent`]: one of bb's 35 provider event types.
///
/// `#[serde(tag = "type")]` makes the discriminant a field of the flattened
/// event, and the explicit `rename`s pin the exact contract strings.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all_fields = "camelCase")]
pub enum ProviderEvent {
    /// The thread began.
    #[serde(rename = "thread/started")]
    ThreadStarted {},
    /// The provider's own identity for the thread.
    #[serde(rename = "thread/identity")]
    ThreadIdentity {
        /// The provider's thread/session id.
        provider_thread_id: String,
    },
    /// A turn began.
    #[serde(rename = "turn/started")]
    TurnStarted {
        /// The provider's thread/session id.
        provider_thread_id: String,
        /// The tool call this turn belongs to, when nested.
        #[serde(skip_serializing_if = "Option::is_none")]
        parent_tool_call_id: Option<String>,
    },
    /// A turn ended. The single terminal event of a run.
    #[serde(rename = "turn/completed")]
    TurnCompleted {
        /// The provider's thread/session id. Required, nullable.
        provider_thread_id: Option<String>,
        /// How it ended.
        status: TurnStatus,
        /// Why, when it did not complete.
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<TurnError>,
        /// A provider checkpoint to resume from.
        #[serde(skip_serializing_if = "Option::is_none")]
        provider_checkpoint_id: Option<String>,
    },
    /// A queued input was accepted.
    #[serde(rename = "turn/input/accepted")]
    TurnInputAccepted {
        /// The client request it answers.
        client_request_id: String,
        /// The provider's thread/session id.
        provider_thread_id: String,
    },
    /// The thread's display name changed.
    #[serde(rename = "thread/name/updated")]
    ThreadNameUpdated {
        /// The provider's thread/session id.
        provider_thread_id: String,
        /// The new name.
        thread_name: String,
    },
    /// The thread's context was compacted.
    #[serde(rename = "thread/compacted")]
    ThreadCompacted {
        /// The provider's thread/session id.
        provider_thread_id: String,
    },
    /// The thread's context was cleared.
    #[serde(rename = "thread/context/cleared")]
    ThreadContextCleared {
        /// The provider's thread/session id.
        provider_thread_id: String,
    },
    /// The thread's goal changed.
    #[serde(rename = "thread/goal/updated")]
    ThreadGoalUpdated {
        /// The provider's thread/session id.
        provider_thread_id: String,
        /// The goal.
        objective: String,
        /// Its status.
        status: GoalStatus,
        /// Seconds spent on it.
        time_used_seconds: f64,
        /// The token budget, when any.
        token_budget: Option<f64>,
        /// Tokens spent so far.
        tokens_used: f64,
    },
    /// The thread's goal was cleared.
    #[serde(rename = "thread/goal/cleared")]
    ThreadGoalCleared {
        /// The provider's thread/session id.
        provider_thread_id: String,
    },
    /// An item began.
    #[serde(rename = "item/started")]
    ItemStarted {
        /// The item.
        item: ThreadEventItem,
        /// The provider's thread/session id.
        provider_thread_id: String,
    },
    /// An item finished.
    #[serde(rename = "item/completed")]
    ItemCompleted {
        /// The item.
        item: ThreadEventItem,
        /// The provider's thread/session id.
        provider_thread_id: String,
    },
    /// A chunk of the assistant's answer.
    #[serde(rename = "item/agentMessage/delta")]
    ItemAgentMessageDelta {
        /// The item this chunk belongs to.
        item_id: String,
        /// The chunk.
        delta: String,
        /// The provider's thread/session id.
        provider_thread_id: String,
        /// The tool call this chunk belongs to, when nested.
        #[serde(skip_serializing_if = "Option::is_none")]
        parent_tool_call_id: Option<String>,
    },
    /// A chunk (or snapshot) of a command's output.
    #[serde(rename = "item/commandExecution/outputDelta")]
    ItemCommandExecutionOutputDelta {
        /// The item this chunk belongs to.
        item_id: String,
        /// The chunk, or a full snapshot when `reset` is set.
        delta: String,
        /// The provider's thread/session id.
        provider_thread_id: String,
        /// Whether `delta` replaces the accumulated output.
        #[serde(skip_serializing_if = "Option::is_none")]
        reset: Option<bool>,
        /// The tool call this chunk belongs to, when nested.
        #[serde(skip_serializing_if = "Option::is_none")]
        parent_tool_call_id: Option<String>,
    },
    /// A chunk of a file change's output.
    #[serde(rename = "item/fileChange/outputDelta")]
    ItemFileChangeOutputDelta {
        /// The item this chunk belongs to.
        item_id: String,
        /// The chunk.
        delta: String,
        /// The provider's thread/session id.
        provider_thread_id: String,
        /// The tool call this chunk belongs to, when nested.
        #[serde(skip_serializing_if = "Option::is_none")]
        parent_tool_call_id: Option<String>,
    },
    /// A chunk of the reasoning summary.
    #[serde(rename = "item/reasoning/summaryTextDelta")]
    ItemReasoningSummaryTextDelta {
        /// The item this chunk belongs to.
        item_id: String,
        /// The chunk.
        delta: String,
        /// The provider's thread/session id.
        provider_thread_id: String,
        /// The tool call this chunk belongs to, when nested.
        #[serde(skip_serializing_if = "Option::is_none")]
        parent_tool_call_id: Option<String>,
    },
    /// A chunk of reasoning text.
    #[serde(rename = "item/reasoning/textDelta")]
    ItemReasoningTextDelta {
        /// The item this chunk belongs to.
        item_id: String,
        /// The chunk.
        delta: String,
        /// The provider's thread/session id.
        provider_thread_id: String,
        /// The tool call this chunk belongs to, when nested.
        #[serde(skip_serializing_if = "Option::is_none")]
        parent_tool_call_id: Option<String>,
    },
    /// A chunk of a plan.
    #[serde(rename = "item/plan/delta")]
    ItemPlanDelta {
        /// The item this chunk belongs to.
        item_id: String,
        /// The chunk.
        delta: String,
        /// The provider's thread/session id.
        provider_thread_id: String,
        /// The tool call this chunk belongs to, when nested.
        #[serde(skip_serializing_if = "Option::is_none")]
        parent_tool_call_id: Option<String>,
    },
    /// Progress from an MCP tool call.
    #[serde(rename = "item/mcpToolCall/progress")]
    ItemMcpToolCallProgress {
        /// The item this progress belongs to.
        item_id: String,
        /// The progress message.
        #[serde(skip_serializing_if = "Option::is_none")]
        message: Option<String>,
        /// The provider's thread/session id.
        provider_thread_id: String,
        /// The tool call this progress belongs to, when nested.
        #[serde(skip_serializing_if = "Option::is_none")]
        parent_tool_call_id: Option<String>,
    },
    /// Progress from a tool call.
    #[serde(rename = "item/toolCall/progress")]
    ItemToolCallProgress {
        /// The item this progress belongs to.
        item_id: String,
        /// The progress message.
        #[serde(skip_serializing_if = "Option::is_none")]
        message: Option<String>,
        /// The provider's thread/session id.
        provider_thread_id: String,
        /// The tool call this progress belongs to, when nested.
        #[serde(skip_serializing_if = "Option::is_none")]
        parent_tool_call_id: Option<String>,
    },
    /// Progress from a background task.
    #[serde(rename = "item/backgroundTask/progress")]
    ItemBackgroundTaskProgress {
        /// The task item.
        item: ThreadEventItem,
        /// The provider's thread/session id.
        provider_thread_id: String,
    },
    /// A background task finished.
    #[serde(rename = "item/backgroundTask/completed")]
    ItemBackgroundTaskCompleted {
        /// The task item.
        item: ThreadEventItem,
        /// The provider's thread/session id.
        provider_thread_id: String,
    },
    /// Progress from a delegated child agent.
    #[serde(rename = "item/delegation/progress")]
    ItemDelegationProgress {
        /// The delegation item.
        item: ThreadEventItem,
        /// The provider's thread/session id.
        provider_thread_id: String,
    },
    /// A delegated child agent finished.
    #[serde(rename = "item/delegation/completed")]
    ItemDelegationCompleted {
        /// The delegation item.
        item: ThreadEventItem,
        /// The provider's thread/session id.
        provider_thread_id: String,
    },
    /// Thread-level token usage changed.
    #[serde(rename = "thread/tokenUsage/updated")]
    ThreadTokenUsageUpdated {
        /// The provider's thread/session id.
        provider_thread_id: String,
        /// The new usage.
        token_usage: ThreadTokenUsage,
    },
    /// Context-window occupancy changed.
    #[serde(rename = "thread/contextWindowUsage/updated")]
    ThreadContextWindowUsageUpdated {
        /// The provider's thread/session id.
        provider_thread_id: String,
        /// The new occupancy.
        context_window_usage: ContextWindowUsage,
    },
    /// The turn's plan changed.
    #[serde(rename = "turn/plan/updated")]
    TurnPlanUpdated {
        /// The provider's thread/session id.
        provider_thread_id: String,
        /// The plan.
        plan: Vec<PlanStep>,
        /// Why the plan changed.
        #[serde(skip_serializing_if = "Option::is_none")]
        explanation: Option<String>,
    },
    /// The turn's working-tree diff changed.
    #[serde(rename = "turn/diff/updated")]
    TurnDiffUpdated {
        /// The provider's thread/session id.
        provider_thread_id: String,
        /// The diff.
        #[serde(skip_serializing_if = "Option::is_none")]
        diff: Option<String>,
    },
    /// The provider reported an error.
    #[serde(rename = "provider/error")]
    ProviderError {
        /// The provider's thread/session id.
        provider_thread_id: String,
        /// The headline message.
        message: String,
        /// The detail.
        #[serde(skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
        /// A structured classification.
        #[serde(skip_serializing_if = "Option::is_none")]
        error_info: Option<ProviderErrorInfo>,
        /// Whether the provider will retry.
        #[serde(skip_serializing_if = "Option::is_none")]
        will_retry: Option<bool>,
    },
    /// Provider rate limits changed.
    #[serde(rename = "provider/rateLimits/updated")]
    ProviderRateLimitsUpdated {
        /// The provider's thread/session id.
        provider_thread_id: String,
        /// The rate-limit state, preserved verbatim.
        rate_limits: Value,
    },
    /// The provider resolved its environment.
    #[serde(rename = "provider.env-resolved")]
    ProviderEnvResolved {
        /// The provider's thread/session id.
        provider_thread_id: String,
        /// The resolved entries.
        entries: Vec<EnvResolvedEntry>,
    },
    /// A provider extension's state changed.
    #[serde(rename = "thread/extensionState/updated")]
    ThreadExtensionStateUpdated {
        /// The provider's thread/session id.
        provider_thread_id: String,
        /// The extension kind.
        kind: String,
        /// The extension payload.
        payload: Value,
    },
    /// The provider warned about something.
    #[serde(rename = "provider/warning")]
    ProviderWarning {
        /// The provider's thread/session id.
        provider_thread_id: String,
        /// The category.
        category: ProviderWarningCategory,
        /// A short summary.
        #[serde(skip_serializing_if = "Option::is_none")]
        summary: Option<String>,
        /// The detail.
        #[serde(skip_serializing_if = "Option::is_none")]
        details: Option<String>,
    },
    /// The provider fell back to another model.
    #[serde(rename = "provider/modelFallback")]
    ProviderModelFallback {
        /// The provider's thread/session id.
        provider_thread_id: String,
        /// The model that was refused.
        original_model: String,
        /// The model actually used.
        fallback_model: String,
        /// Why.
        reason: ModelFallbackReason,
        /// A human-readable message.
        message: String,
    },
    /// A provider frame loom does not model.
    ///
    /// This is the contract's own last-resort *provider* event, not a loom
    /// catch-all: bb defines it, and the projection renders it as diagnostic.
    /// loom's bridge emits it only for a provider frame it truly cannot map.
    #[serde(rename = "provider/unhandled")]
    ProviderUnhandled {
        /// The provider's thread/session id.
        provider_thread_id: String,
        /// The provider id, e.g. `pi`.
        provider_id: String,
        /// The raw frame's `type`/method.
        raw_type: String,
        /// The raw frame.
        raw_event: ProviderRawEvent,
        /// The tool call this frame belongs to, when nested.
        #[serde(skip_serializing_if = "Option::is_none")]
        parent_tool_call_id: Option<String>,
    },
}

impl ProviderEvent {
    /// The stable `type` tag, matching the serialized form.
    pub fn kind(&self) -> &'static str {
        match self {
            ProviderEvent::ThreadStarted { .. } => "thread/started",
            ProviderEvent::ThreadIdentity { .. } => "thread/identity",
            ProviderEvent::TurnStarted { .. } => "turn/started",
            ProviderEvent::TurnCompleted { .. } => "turn/completed",
            ProviderEvent::TurnInputAccepted { .. } => "turn/input/accepted",
            ProviderEvent::ThreadNameUpdated { .. } => "thread/name/updated",
            ProviderEvent::ThreadCompacted { .. } => "thread/compacted",
            ProviderEvent::ThreadContextCleared { .. } => "thread/context/cleared",
            ProviderEvent::ThreadGoalUpdated { .. } => "thread/goal/updated",
            ProviderEvent::ThreadGoalCleared { .. } => "thread/goal/cleared",
            ProviderEvent::ItemStarted { .. } => "item/started",
            ProviderEvent::ItemCompleted { .. } => "item/completed",
            ProviderEvent::ItemAgentMessageDelta { .. } => "item/agentMessage/delta",
            ProviderEvent::ItemCommandExecutionOutputDelta { .. } => {
                "item/commandExecution/outputDelta"
            }
            ProviderEvent::ItemFileChangeOutputDelta { .. } => "item/fileChange/outputDelta",
            ProviderEvent::ItemReasoningSummaryTextDelta { .. } => {
                "item/reasoning/summaryTextDelta"
            }
            ProviderEvent::ItemReasoningTextDelta { .. } => "item/reasoning/textDelta",
            ProviderEvent::ItemPlanDelta { .. } => "item/plan/delta",
            ProviderEvent::ItemMcpToolCallProgress { .. } => "item/mcpToolCall/progress",
            ProviderEvent::ItemToolCallProgress { .. } => "item/toolCall/progress",
            ProviderEvent::ItemBackgroundTaskProgress { .. } => "item/backgroundTask/progress",
            ProviderEvent::ItemBackgroundTaskCompleted { .. } => "item/backgroundTask/completed",
            ProviderEvent::ItemDelegationProgress { .. } => "item/delegation/progress",
            ProviderEvent::ItemDelegationCompleted { .. } => "item/delegation/completed",
            ProviderEvent::ThreadTokenUsageUpdated { .. } => "thread/tokenUsage/updated",
            ProviderEvent::ThreadContextWindowUsageUpdated { .. } => {
                "thread/contextWindowUsage/updated"
            }
            ProviderEvent::TurnPlanUpdated { .. } => "turn/plan/updated",
            ProviderEvent::TurnDiffUpdated { .. } => "turn/diff/updated",
            ProviderEvent::ProviderError { .. } => "provider/error",
            ProviderEvent::ProviderRateLimitsUpdated { .. } => "provider/rateLimits/updated",
            ProviderEvent::ProviderEnvResolved { .. } => "provider.env-resolved",
            ProviderEvent::ThreadExtensionStateUpdated { .. } => "thread/extensionState/updated",
            ProviderEvent::ProviderWarning { .. } => "provider/warning",
            ProviderEvent::ProviderModelFallback { .. } => "provider/modelFallback",
            ProviderEvent::ProviderUnhandled { .. } => "provider/unhandled",
        }
    }

    /// Whether this event ends the run.
    ///
    /// Exactly one event does: `turn/completed`. That is the invariant that
    /// keeps a thread from being stuck in `working` after a provider crash.
    pub fn is_terminal(&self) -> bool {
        matches!(self, ProviderEvent::TurnCompleted { .. })
    }

    /// The provider's thread/session id, when the event carries one.
    pub fn provider_thread_id(&self) -> Option<&str> {
        match self {
            ProviderEvent::ThreadIdentity { provider_thread_id }
            | ProviderEvent::TurnStarted {
                provider_thread_id, ..
            }
            | ProviderEvent::TurnInputAccepted {
                provider_thread_id, ..
            }
            | ProviderEvent::ThreadNameUpdated {
                provider_thread_id, ..
            }
            | ProviderEvent::ThreadCompacted { provider_thread_id }
            | ProviderEvent::ThreadContextCleared { provider_thread_id }
            | ProviderEvent::ThreadGoalUpdated {
                provider_thread_id, ..
            }
            | ProviderEvent::ThreadGoalCleared { provider_thread_id }
            | ProviderEvent::ItemStarted {
                provider_thread_id, ..
            }
            | ProviderEvent::ItemCompleted {
                provider_thread_id, ..
            }
            | ProviderEvent::ItemAgentMessageDelta {
                provider_thread_id, ..
            }
            | ProviderEvent::ItemCommandExecutionOutputDelta {
                provider_thread_id, ..
            }
            | ProviderEvent::ItemFileChangeOutputDelta {
                provider_thread_id, ..
            }
            | ProviderEvent::ItemReasoningSummaryTextDelta {
                provider_thread_id, ..
            }
            | ProviderEvent::ItemReasoningTextDelta {
                provider_thread_id, ..
            }
            | ProviderEvent::ItemPlanDelta {
                provider_thread_id, ..
            }
            | ProviderEvent::ItemMcpToolCallProgress {
                provider_thread_id, ..
            }
            | ProviderEvent::ItemToolCallProgress {
                provider_thread_id, ..
            }
            | ProviderEvent::ItemBackgroundTaskProgress {
                provider_thread_id, ..
            }
            | ProviderEvent::ItemBackgroundTaskCompleted {
                provider_thread_id, ..
            }
            | ProviderEvent::ItemDelegationProgress {
                provider_thread_id, ..
            }
            | ProviderEvent::ItemDelegationCompleted {
                provider_thread_id, ..
            }
            | ProviderEvent::ThreadTokenUsageUpdated {
                provider_thread_id, ..
            }
            | ProviderEvent::ThreadContextWindowUsageUpdated {
                provider_thread_id, ..
            }
            | ProviderEvent::TurnPlanUpdated {
                provider_thread_id, ..
            }
            | ProviderEvent::TurnDiffUpdated {
                provider_thread_id, ..
            }
            | ProviderEvent::ProviderError {
                provider_thread_id, ..
            }
            | ProviderEvent::ProviderRateLimitsUpdated {
                provider_thread_id, ..
            }
            | ProviderEvent::ProviderEnvResolved {
                provider_thread_id, ..
            }
            | ProviderEvent::ThreadExtensionStateUpdated {
                provider_thread_id, ..
            }
            | ProviderEvent::ProviderWarning {
                provider_thread_id, ..
            }
            | ProviderEvent::ProviderModelFallback {
                provider_thread_id, ..
            }
            | ProviderEvent::ProviderUnhandled {
                provider_thread_id, ..
            } => Some(provider_thread_id),
            ProviderEvent::TurnCompleted {
                provider_thread_id, ..
            } => provider_thread_id.as_deref(),
            ProviderEvent::ThreadStarted { .. } => None,
        }
    }
}

/// One contract thread event: the flattened object bb stores and serves.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ThreadEvent {
    /// The thread the event belongs to.
    #[serde(rename = "threadId")]
    pub thread_id: ThreadId,
    /// Which room it belongs to.
    pub scope: ThreadEventScope,
    /// The event body.
    #[serde(flatten)]
    pub body: ProviderEvent,
}

impl ThreadEvent {
    /// Builds an event.
    pub fn new(thread_id: ThreadId, scope: ThreadEventScope, body: ProviderEvent) -> Self {
        Self {
            thread_id,
            scope,
            body,
        }
    }

    /// Builds a turn-scoped event for `run_id`.
    pub fn for_turn(thread_id: ThreadId, run_id: &RunId, body: ProviderEvent) -> Self {
        Self::new(thread_id, ThreadEventScope::turn(run_id), body)
    }

    /// The stable `type` tag, matching the serialized form.
    pub fn kind(&self) -> &'static str {
        self.body.kind()
    }

    /// Whether this event ends the run.
    pub fn is_terminal(&self) -> bool {
        self.body.is_terminal()
    }

    /// The provider's thread/session id, when the event carries one.
    pub fn provider_thread_id(&self) -> Option<&str> {
        self.body.provider_thread_id()
    }
}

/// The provider error a decode can produce.
///
/// There is deliberately no "unknown event" fallback variant: a frame this
/// build cannot model is an error, because silently inventing a timeline row
/// is worse than a visible failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProviderEventError {
    /// A `type` tag outside the contract's union.
    UnknownType(String),
    /// A `system/*` or `client/*` type, which no provider produces.
    NotAPProviderEvent(String),
}

impl std::fmt::Display for ProviderEventError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProviderEventError::UnknownType(kind) => {
                write!(f, "`{kind}` is not a contract thread event type")
            }
            ProviderEventError::NotAPProviderEvent(kind) => write!(
                f,
                "`{kind}` is a client/system thread event and has no provider body"
            ),
        }
    }
}

impl std::error::Error for ProviderEventError {}

/// The 35 provider event discriminators, as an exhaustive enum.
///
/// Kept separate from [`ProviderEvent`] so code can enumerate and validate the
/// contract's type set (for example when checking that loom models every one)
/// without constructing an event.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ProviderEventType {
    /// `thread/started`
    ThreadStarted,
    /// `thread/identity`
    ThreadIdentity,
    /// `turn/started`
    TurnStarted,
    /// `turn/completed`
    TurnCompleted,
    /// `turn/input/accepted`
    TurnInputAccepted,
    /// `thread/name/updated`
    ThreadNameUpdated,
    /// `thread/compacted`
    ThreadCompacted,
    /// `thread/context/cleared`
    ThreadContextCleared,
    /// `thread/goal/updated`
    ThreadGoalUpdated,
    /// `thread/goal/cleared`
    ThreadGoalCleared,
    /// `item/started`
    ItemStarted,
    /// `item/completed`
    ItemCompleted,
    /// `item/agentMessage/delta`
    ItemAgentMessageDelta,
    /// `item/commandExecution/outputDelta`
    ItemCommandExecutionOutputDelta,
    /// `item/fileChange/outputDelta`
    ItemFileChangeOutputDelta,
    /// `item/reasoning/summaryTextDelta`
    ItemReasoningSummaryTextDelta,
    /// `item/reasoning/textDelta`
    ItemReasoningTextDelta,
    /// `item/plan/delta`
    ItemPlanDelta,
    /// `item/mcpToolCall/progress`
    ItemMcpToolCallProgress,
    /// `item/toolCall/progress`
    ItemToolCallProgress,
    /// `item/backgroundTask/progress`
    ItemBackgroundTaskProgress,
    /// `item/backgroundTask/completed`
    ItemBackgroundTaskCompleted,
    /// `item/delegation/progress`
    ItemDelegationProgress,
    /// `item/delegation/completed`
    ItemDelegationCompleted,
    /// `thread/tokenUsage/updated`
    ThreadTokenUsageUpdated,
    /// `thread/contextWindowUsage/updated`
    ThreadContextWindowUsageUpdated,
    /// `turn/plan/updated`
    TurnPlanUpdated,
    /// `turn/diff/updated`
    TurnDiffUpdated,
    /// `provider/error`
    ProviderError,
    /// `provider/rateLimits/updated`
    ProviderRateLimitsUpdated,
    /// `provider.env-resolved`
    ProviderEnvResolved,
    /// `thread/extensionState/updated`
    ThreadExtensionStateUpdated,
    /// `provider/warning`
    ProviderWarning,
    /// `provider/modelFallback`
    ProviderModelFallback,
    /// `provider/unhandled`
    ProviderUnhandled,
}

/// The union bb declares: the 35 [`ProviderEventType`] values plus the 13
/// `client/*` and `system/*` types no provider produces.
///
/// The extra 13 are enumerated so an incoming frame can be classified exactly
/// — as "a real contract type that is not a provider event", not as unknown.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ThreadEventType {
    /// One of the 35 provider event types.
    Provider(ProviderEventType),
    /// `client/thread/start`
    ClientThreadStart,
    /// `client/turn/requested`
    ClientTurnRequested,
    /// `client/turn/rejected`
    ClientTurnRejected,
    /// `client/turn/start`
    ClientTurnStart,
    /// `system/error`
    SystemError,
    /// `system/manager/user_message`
    SystemManagerUserMessage,
    /// `system/thread/interrupted`
    SystemThreadInterrupted,
    /// `system/operation`
    SystemOperation,
    /// `system/interaction/lifecycle`
    SystemInteractionLifecycle,
    /// `system/permissionGrant/lifecycle`
    SystemPermissionGrantLifecycle,
    /// `system/userQuestion/lifecycle`
    SystemUserQuestionLifecycle,
    /// `system/thread-provisioning`
    SystemThreadProvisioning,
    /// `system/provider-turn-watchdog`
    SystemProviderTurnWatchdog,
}

impl ProviderEventType {
    /// Every provider event type, in the contract's declared order.
    pub const ALL: [ProviderEventType; 35] = [
        ProviderEventType::ThreadStarted,
        ProviderEventType::ThreadIdentity,
        ProviderEventType::TurnStarted,
        ProviderEventType::TurnCompleted,
        ProviderEventType::TurnInputAccepted,
        ProviderEventType::ThreadNameUpdated,
        ProviderEventType::ThreadCompacted,
        ProviderEventType::ThreadContextCleared,
        ProviderEventType::ThreadGoalUpdated,
        ProviderEventType::ThreadGoalCleared,
        ProviderEventType::ItemStarted,
        ProviderEventType::ItemCompleted,
        ProviderEventType::ItemAgentMessageDelta,
        ProviderEventType::ItemCommandExecutionOutputDelta,
        ProviderEventType::ItemFileChangeOutputDelta,
        ProviderEventType::ItemReasoningSummaryTextDelta,
        ProviderEventType::ItemReasoningTextDelta,
        ProviderEventType::ItemPlanDelta,
        ProviderEventType::ItemMcpToolCallProgress,
        ProviderEventType::ItemToolCallProgress,
        ProviderEventType::ItemBackgroundTaskProgress,
        ProviderEventType::ItemBackgroundTaskCompleted,
        ProviderEventType::ItemDelegationProgress,
        ProviderEventType::ItemDelegationCompleted,
        ProviderEventType::ThreadTokenUsageUpdated,
        ProviderEventType::ThreadContextWindowUsageUpdated,
        ProviderEventType::TurnPlanUpdated,
        ProviderEventType::TurnDiffUpdated,
        ProviderEventType::ProviderError,
        ProviderEventType::ProviderRateLimitsUpdated,
        ProviderEventType::ProviderEnvResolved,
        ProviderEventType::ThreadExtensionStateUpdated,
        ProviderEventType::ProviderWarning,
        ProviderEventType::ProviderModelFallback,
        ProviderEventType::ProviderUnhandled,
    ];

    /// The exact wire token.
    pub fn as_str(self) -> &'static str {
        match self {
            ProviderEventType::ThreadStarted => "thread/started",
            ProviderEventType::ThreadIdentity => "thread/identity",
            ProviderEventType::TurnStarted => "turn/started",
            ProviderEventType::TurnCompleted => "turn/completed",
            ProviderEventType::TurnInputAccepted => "turn/input/accepted",
            ProviderEventType::ThreadNameUpdated => "thread/name/updated",
            ProviderEventType::ThreadCompacted => "thread/compacted",
            ProviderEventType::ThreadContextCleared => "thread/context/cleared",
            ProviderEventType::ThreadGoalUpdated => "thread/goal/updated",
            ProviderEventType::ThreadGoalCleared => "thread/goal/cleared",
            ProviderEventType::ItemStarted => "item/started",
            ProviderEventType::ItemCompleted => "item/completed",
            ProviderEventType::ItemAgentMessageDelta => "item/agentMessage/delta",
            ProviderEventType::ItemCommandExecutionOutputDelta => {
                "item/commandExecution/outputDelta"
            }
            ProviderEventType::ItemFileChangeOutputDelta => "item/fileChange/outputDelta",
            ProviderEventType::ItemReasoningSummaryTextDelta => "item/reasoning/summaryTextDelta",
            ProviderEventType::ItemReasoningTextDelta => "item/reasoning/textDelta",
            ProviderEventType::ItemPlanDelta => "item/plan/delta",
            ProviderEventType::ItemMcpToolCallProgress => "item/mcpToolCall/progress",
            ProviderEventType::ItemToolCallProgress => "item/toolCall/progress",
            ProviderEventType::ItemBackgroundTaskProgress => "item/backgroundTask/progress",
            ProviderEventType::ItemBackgroundTaskCompleted => "item/backgroundTask/completed",
            ProviderEventType::ItemDelegationProgress => "item/delegation/progress",
            ProviderEventType::ItemDelegationCompleted => "item/delegation/completed",
            ProviderEventType::ThreadTokenUsageUpdated => "thread/tokenUsage/updated",
            ProviderEventType::ThreadContextWindowUsageUpdated => {
                "thread/contextWindowUsage/updated"
            }
            ProviderEventType::TurnPlanUpdated => "turn/plan/updated",
            ProviderEventType::TurnDiffUpdated => "turn/diff/updated",
            ProviderEventType::ProviderError => "provider/error",
            ProviderEventType::ProviderRateLimitsUpdated => "provider/rateLimits/updated",
            ProviderEventType::ProviderEnvResolved => "provider.env-resolved",
            ProviderEventType::ThreadExtensionStateUpdated => "thread/extensionState/updated",
            ProviderEventType::ProviderWarning => "provider/warning",
            ProviderEventType::ProviderModelFallback => "provider/modelFallback",
            ProviderEventType::ProviderUnhandled => "provider/unhandled",
        }
    }
}

impl ThreadEventType {
    /// Every contract type: 35 provider plus 13 client/system.
    pub const ALL: [ThreadEventType; 48] = [
        ThreadEventType::Provider(ProviderEventType::ThreadStarted),
        ThreadEventType::Provider(ProviderEventType::ThreadIdentity),
        ThreadEventType::Provider(ProviderEventType::TurnStarted),
        ThreadEventType::Provider(ProviderEventType::TurnCompleted),
        ThreadEventType::Provider(ProviderEventType::TurnInputAccepted),
        ThreadEventType::Provider(ProviderEventType::ThreadNameUpdated),
        ThreadEventType::Provider(ProviderEventType::ThreadCompacted),
        ThreadEventType::Provider(ProviderEventType::ThreadContextCleared),
        ThreadEventType::Provider(ProviderEventType::ThreadGoalUpdated),
        ThreadEventType::Provider(ProviderEventType::ThreadGoalCleared),
        ThreadEventType::Provider(ProviderEventType::ItemStarted),
        ThreadEventType::Provider(ProviderEventType::ItemCompleted),
        ThreadEventType::Provider(ProviderEventType::ItemAgentMessageDelta),
        ThreadEventType::Provider(ProviderEventType::ItemCommandExecutionOutputDelta),
        ThreadEventType::Provider(ProviderEventType::ItemFileChangeOutputDelta),
        ThreadEventType::Provider(ProviderEventType::ItemReasoningSummaryTextDelta),
        ThreadEventType::Provider(ProviderEventType::ItemReasoningTextDelta),
        ThreadEventType::Provider(ProviderEventType::ItemPlanDelta),
        ThreadEventType::Provider(ProviderEventType::ItemMcpToolCallProgress),
        ThreadEventType::Provider(ProviderEventType::ItemToolCallProgress),
        ThreadEventType::Provider(ProviderEventType::ItemBackgroundTaskProgress),
        ThreadEventType::Provider(ProviderEventType::ItemBackgroundTaskCompleted),
        ThreadEventType::Provider(ProviderEventType::ItemDelegationProgress),
        ThreadEventType::Provider(ProviderEventType::ItemDelegationCompleted),
        ThreadEventType::Provider(ProviderEventType::ThreadTokenUsageUpdated),
        ThreadEventType::Provider(ProviderEventType::ThreadContextWindowUsageUpdated),
        ThreadEventType::Provider(ProviderEventType::TurnPlanUpdated),
        ThreadEventType::Provider(ProviderEventType::TurnDiffUpdated),
        ThreadEventType::Provider(ProviderEventType::ProviderError),
        ThreadEventType::Provider(ProviderEventType::ProviderRateLimitsUpdated),
        ThreadEventType::Provider(ProviderEventType::ProviderEnvResolved),
        ThreadEventType::Provider(ProviderEventType::ThreadExtensionStateUpdated),
        ThreadEventType::Provider(ProviderEventType::ProviderWarning),
        ThreadEventType::Provider(ProviderEventType::ProviderModelFallback),
        ThreadEventType::Provider(ProviderEventType::ProviderUnhandled),
        ThreadEventType::ClientThreadStart,
        ThreadEventType::ClientTurnRequested,
        ThreadEventType::ClientTurnRejected,
        ThreadEventType::ClientTurnStart,
        ThreadEventType::SystemError,
        ThreadEventType::SystemManagerUserMessage,
        ThreadEventType::SystemThreadInterrupted,
        ThreadEventType::SystemOperation,
        ThreadEventType::SystemInteractionLifecycle,
        ThreadEventType::SystemPermissionGrantLifecycle,
        ThreadEventType::SystemUserQuestionLifecycle,
        ThreadEventType::SystemThreadProvisioning,
        ThreadEventType::SystemProviderTurnWatchdog,
    ];

    /// The exact wire token.
    pub fn as_str(self) -> &'static str {
        match self {
            ThreadEventType::Provider(provider) => provider.as_str(),
            ThreadEventType::ClientThreadStart => "client/thread/start",
            ThreadEventType::ClientTurnRequested => "client/turn/requested",
            ThreadEventType::ClientTurnRejected => "client/turn/rejected",
            ThreadEventType::ClientTurnStart => "client/turn/start",
            ThreadEventType::SystemError => "system/error",
            ThreadEventType::SystemManagerUserMessage => "system/manager/user_message",
            ThreadEventType::SystemThreadInterrupted => "system/thread/interrupted",
            ThreadEventType::SystemOperation => "system/operation",
            ThreadEventType::SystemInteractionLifecycle => "system/interaction/lifecycle",
            ThreadEventType::SystemPermissionGrantLifecycle => "system/permissionGrant/lifecycle",
            ThreadEventType::SystemUserQuestionLifecycle => "system/userQuestion/lifecycle",
            ThreadEventType::SystemThreadProvisioning => "system/thread-provisioning",
            ThreadEventType::SystemProviderTurnWatchdog => "system/provider-turn-watchdog",
        }
    }

    /// Parses a wire token into any contract type, provider or not.
    ///
    /// A `client/*` or `system/*` token parses successfully: it *is* a
    /// contract type, it is simply not one a provider body exists for. Use
    /// [`ThreadEventType::provider`] to get the provider variant, which
    /// rejects those explicitly.
    pub fn parse(kind: &str) -> Result<Self, ProviderEventError> {
        ThreadEventType::ALL
            .iter()
            .find(|candidate| candidate.as_str() == kind)
            .copied()
            .ok_or_else(|| ProviderEventError::UnknownType(kind.to_owned()))
    }

    /// The provider variant, or an explicit error for a client/system type.
    pub fn provider(self) -> Result<ProviderEventType, ProviderEventError> {
        match self {
            ThreadEventType::Provider(provider) => Ok(provider),
            other => Err(ProviderEventError::NotAPProviderEvent(
                other.as_str().to_owned(),
            )),
        }
    }

    /// Whether a provider can produce this type.
    pub fn is_provider_event(self) -> bool {
        matches!(self, ThreadEventType::Provider(_))
    }

    /// The provider type for a wire token, rejecting client/system types.
    pub fn parse_provider(kind: &str) -> Result<ProviderEventType, ProviderEventError> {
        ThreadEventType::parse(kind)?.provider()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn thread_id() -> ThreadId {
        ThreadId::mint()
    }

    #[test]
    fn every_provider_type_has_a_variant_and_a_token() {
        // `ProviderEventType::ALL` is the source of truth for the contract's
        // 35 values; each token must be unique.
        let mut tokens: Vec<&str> = ProviderEventType::ALL.iter().map(|t| t.as_str()).collect();
        tokens.sort_unstable();
        tokens.dedup();
        assert_eq!(tokens.len(), 35);
    }

    #[test]
    fn the_type_set_is_35_provider_plus_13_system() {
        assert_eq!(ProviderEventType::ALL.len(), 35);
        assert_eq!(ThreadEventType::ALL.len(), 48);
        let provider = ThreadEventType::ALL
            .iter()
            .filter(|t| t.is_provider_event())
            .count();
        assert_eq!(provider, 35);
    }

    #[test]
    fn a_contract_type_is_classified_not_called_unknown() {
        // A real contract type parses, and asking for its provider body is the
        // explicit, typed rejection.
        let system = ThreadEventType::parse("system/error").unwrap();
        assert_eq!(system, ThreadEventType::SystemError);
        assert!(matches!(
            system.provider(),
            Err(ProviderEventError::NotAPProviderEvent(_))
        ));
        assert!(matches!(
            ThreadEventType::parse_provider("system/error"),
            Err(ProviderEventError::NotAPProviderEvent(_))
        ));

        // A token outside the union is unknown.
        assert!(matches!(
            ThreadEventType::parse("provider/nonsense"),
            Err(ProviderEventError::UnknownType(_))
        ));
        assert_eq!(
            ThreadEventType::parse_provider("provider/warning").unwrap(),
            ProviderEventType::ProviderWarning
        );
    }

    #[test]
    fn a_delta_event_serializes_to_the_contract_shape() {
        let event = ThreadEvent::for_turn(
            thread_id(),
            &RunId::mint(),
            ProviderEvent::ItemAgentMessageDelta {
                item_id: "assistant-1".into(),
                delta: "hello".into(),
                provider_thread_id: "thr_session".into(),
                parent_tool_call_id: None,
            },
        );
        let value = serde_json::to_value(&event).unwrap();
        assert_eq!(value["type"], "item/agentMessage/delta");
        assert_eq!(value["delta"], "hello");
        assert_eq!(value["itemId"], "assistant-1");
        assert_eq!(value["providerThreadId"], "thr_session");
        assert_eq!(value["scope"]["kind"], "turn");
        assert!(value.get("parentToolCallId").is_none());
        assert!(value.get("threadId").is_some());
    }

    #[test]
    fn a_nullable_required_field_serializes_as_null() {
        // `turn/completed.providerThreadId` is `string | null` and required.
        let event = ThreadEvent::for_turn(
            thread_id(),
            &RunId::mint(),
            ProviderEvent::TurnCompleted {
                provider_thread_id: None,
                status: TurnStatus::Failed,
                error: Some(TurnError {
                    message: "boom".into(),
                }),
                provider_checkpoint_id: None,
            },
        );
        let value = serde_json::to_value(&event).unwrap();
        assert!(value.get("providerThreadId").is_some());
        assert!(value["providerThreadId"].is_null());
        assert_eq!(value["status"], "failed");
        assert_eq!(value["error"]["message"], "boom");
    }

    #[test]
    fn only_turn_completed_is_terminal() {
        let completed = ProviderEvent::TurnCompleted {
            provider_thread_id: Some("p".into()),
            status: TurnStatus::Completed,
            error: None,
            provider_checkpoint_id: None,
        };
        assert!(completed.is_terminal());
        let started = ProviderEvent::TurnStarted {
            provider_thread_id: "p".into(),
            parent_tool_call_id: None,
        };
        assert!(!started.is_terminal());
    }

    #[test]
    fn an_item_round_trips_with_its_kind() {
        let item = ThreadEventItem::CommandExecution {
            id: "tool-1".into(),
            command: "ls".into(),
            cwd: "/srv".into(),
            status: ItemStatus::Completed,
            approval_status: None,
            aggregated_output: Some("a\nb".into()),
            exit_code: Some(0),
            duration_ms: Some(12.0),
            presentation: None,
            parent_tool_call_id: None,
        };
        let value = serde_json::to_value(&item).unwrap();
        assert_eq!(value["type"], "commandExecution");
        assert!(value["approvalStatus"].is_null());
        assert_eq!(value["aggregatedOutput"], "a\nb");
        let decoded: ThreadEventItem = serde_json::from_value(value).unwrap();
        assert_eq!(decoded, item);
    }
}
