//! Translating ACP session updates into loom's contract events.
//!
//! The adapter is a **translator with state**. ACP models a session and a
//! prompt; loom's contract models threads, turns and *items* with stable ids
//! that a later event refers back to. Holding that mapping is what lets the
//! projection pair a delta with the `item/started` that opened it.
//!
//! Three rules shape everything here, and each one is a place a careless
//! implementation goes wrong:
//!
//! 1. **ACP tool calls are generic, loom's items are specific.** ACP gives a
//!    `title`, a `kind`, an optional free-form `raw_input` and a `content`
//!    array. The contract's `CommandExecution` has a *required* `command` and
//!    `cwd`; `FileChange` requires `changes[]` with paths; `Search` requires
//!    `mode` and `query`. So a variant is chosen only when its required data is
//!    actually present, and otherwise the generic `ToolCall` item is used —
//!    which needs only `id`, `tool` and `status` and is the *accurate*
//!    representation of a tool loom cannot interpret. Nothing is ever
//!    fabricated to satisfy a shape.
//!
//! 2. **`UserMessageChunk` is not an assistant delta.** The contract's
//!    `item/agentMessage/delta` is the assistant's channel; ACP echoes the
//!    user's own message through `UserMessageChunk`. Routing it into the same
//!    event would render the user's text as the agent's answer, so it becomes a
//!    user message item instead.
//!
//! 3. **Every run ends in exactly one terminal event.** loom's run lifecycle
//!    depends on it: a run without one leaves its thread stuck in `working`.
//!    ACP signals completion differently per protocol version, so both funnel
//!    through [`AcpTranslator::on_stop_reason`].
//!
//! See `docs/acp-adapter.md` for the full mapping and its rationale.

use std::collections::{BTreeMap, HashMap};

use agent_client_protocol_schema::v1::{
    ContentBlock, ContentChunk, Plan, PlanEntryStatus, SessionInfoUpdate, SessionUpdate,
    StopReason, ToolCall, ToolCallContent, ToolCallStatus, ToolCallUpdate, ToolKind, UsageUpdate,
};
use loom_domain::{
    ItemStatus, PlanStep, PlanStepStatus, ProviderEvent, SearchMode, ThreadEventItem, TurnError,
    TurnStatus, UserContent,
};
use serde_json::Value;

/// What a translator is built from: the parts of a run every event needs.
pub struct RunContext {
    /// loom's thread id, which is also the provider session identity.
    pub thread_id: loom_domain::ThreadId,
    /// The workspace the provider runs in, used as a tool item's `cwd`.
    pub cwd: Option<String>,
}

/// One run's ACP frames, translated into contract bodies.
///
/// Public so tests (and the transport) can drive the translation directly
/// instead of through a live agent.
pub struct AcpTranslator {
    ctx: RunContext,
    /// Whether `thread/identity` has been emitted. ACP has no notion of a
    /// running thread, so this is accounted for here rather than observed.
    identified: bool,
    /// Whether a turn is open, so `turn/started` is emitted once per turn.
    turn_open: bool,
    /// Counters for minting item ids, which ACP supplies only for tool calls.
    assistant_seq: u64,
    user_seq: u64,
    reasoning_seq: u64,
    plan_seq: u64,
    /// The item id of the assistant message currently streaming.
    assistant_id: Option<String>,
    /// The item id of each thinking block, keyed by ACP's `contentIndex`: ACP
    /// does not identify thought chunks, so the index is the only grouping.
    thinking_ids: HashMap<u64, String>,
    /// Tool items by ACP `toolCallId`. ACP sends a full `ToolCall` once and
    /// then `ToolCallUpdate` patches in which every field is optional, so the
    /// merged shape must be retained to emit a whole `item/completed`.
    tools: HashMap<String, ToolItem>,
}

/// A tool call's accumulated state.
struct ToolItem {
    item: ThreadEventItem,
}

/// What an update translates into.
///
/// Plain bodies rather than `RunEvent`s, so stamping and wrapping stay the
/// caller's decision and the translation is testable in isolation.
pub type Translated = Vec<ProviderEvent>;

impl AcpTranslator {
    /// Builds a translator for one run.
    pub fn new(ctx: RunContext) -> Self {
        Self {
            ctx,
            identified: false,
            turn_open: false,
            assistant_seq: 0,
            user_seq: 0,
            reasoning_seq: 0,
            plan_seq: 0,
            assistant_id: None,
            thinking_ids: HashMap::new(),
            tools: HashMap::new(),
        }
    }

    /// The provider thread id reported on every event.
    ///
    /// loom runs one provider session per thread, so the thread id is the
    /// session identity; ACP's own `sessionId` is recorded separately for
    /// resume.
    fn ptid(&self) -> String {
        self.ctx.thread_id.to_string()
    }

    /// A prompt has been sent: open the turn.
    ///
    /// ACP has no `turn/started`, so the fact is synthesized here. Identity is
    /// emitted first, once per run.
    pub fn on_prompt_sent(&mut self) -> Translated {
        let mut events = Vec::new();
        if !self.identified {
            self.identified = true;
            events.push(ProviderEvent::ThreadIdentity {
                provider_thread_id: self.ptid(),
            });
        }
        if !self.turn_open {
            self.turn_open = true;
            events.push(ProviderEvent::TurnStarted {
                provider_thread_id: self.ptid(),
                parent_tool_call_id: None,
            });
        }
        events
    }

    /// One ACP session update.
    ///
    /// A variant carrying no timeline fact loom can state yields nothing. That
    /// is deliberate rather than a missing mapping: a mode change is not work,
    /// and config options and the command menu are UI affordances the client
    /// already holds. Emitting anything for them would put a fact in the log
    /// that the provider never stated.
    pub fn on_session_update(&mut self, update: &SessionUpdate) -> Translated {
        match update {
            SessionUpdate::UserMessageChunk(chunk) => self.on_user_chunk(chunk),
            SessionUpdate::AgentMessageChunk(chunk) => self.on_agent_chunk(chunk),
            SessionUpdate::AgentThoughtChunk(chunk) => self.on_thought_chunk(chunk),
            SessionUpdate::ToolCall(call) => self.on_tool_call(call),
            SessionUpdate::ToolCallUpdate(patch) => self.on_tool_update(patch),
            SessionUpdate::Plan(plan) => self.on_plan(plan),
            SessionUpdate::UsageUpdate(usage) => self.on_usage(usage),
            SessionUpdate::SessionInfoUpdate(info) => self.on_session_info(info),
            SessionUpdate::CurrentModeUpdate(_)
            | SessionUpdate::ConfigOptionUpdate(_)
            | SessionUpdate::AvailableCommandsUpdate(_) => Vec::new(),
            // `SessionUpdate` is `#[non_exhaustive]`: a newer schema may add
            // variants. An update loom reaches no arm for produces no event and
            // no fabricated catch-all, the rule W-538 applied to
            // `provider/unhandled`.
            _ => Vec::new(),
        }
    }

    /// The turn finished, in the protocol's terms.
    ///
    /// Both versions funnel here: v1 reports `stop_reason` on the
    /// `session/prompt` response, v2 through `StateUpdate::Idle`. Normalizing
    /// at the call site keeps the mapping in one place.
    pub fn on_stop_reason(&mut self, reason: StopReason) -> Translated {
        let mut events = self.flush_assistant();
        let (status, error) = match reason {
            StopReason::EndTurn => (TurnStatus::Completed, None),
            // A limit was reached, so the model stopped. The run succeeded;
            // continuing is a new turn the caller decides to send, not a
            // failure of this one.
            StopReason::MaxTokens | StopReason::MaxTurnRequests => (TurnStatus::Completed, None),
            StopReason::Refusal => (
                TurnStatus::Failed,
                Some("the agent refused the turn".to_string()),
            ),
            StopReason::Cancelled => (TurnStatus::Interrupted, None),
            // `#[non_exhaustive]`: an unrecognized stop reason is still a stop.
            // Reporting the turn as completed is the accurate reading; the
            // alternative is hanging the thread.
            _ => (TurnStatus::Completed, None),
        };
        self.turn_open = false;
        events.push(ProviderEvent::TurnCompleted {
            provider_thread_id: Some(self.ptid()),
            status,
            error: error.map(|message| TurnError { message }),
            provider_checkpoint_id: None,
        });
        events
    }

    /// A failure that ends the turn out of band: a transport error, a deadline.
    pub fn on_failure(&mut self, message: String) -> Translated {
        let mut events = self.flush_assistant();
        self.turn_open = false;
        events.push(ProviderEvent::TurnCompleted {
            provider_thread_id: Some(self.ptid()),
            status: TurnStatus::Failed,
            error: Some(TurnError { message }),
            provider_checkpoint_id: None,
        });
        events
    }

    // --- messages ---------------------------------------------------------

    fn on_user_chunk(&mut self, chunk: &ContentChunk) -> Translated {
        // Rule 2: this is the user's own message and must not share the
        // assistant's delta channel.
        let text = content_text(&chunk.content);
        if text.is_empty() {
            return Vec::new();
        }
        self.user_seq += 1;
        vec![ProviderEvent::ItemStarted {
            item: ThreadEventItem::UserMessage {
                id: format!("user-{}", self.user_seq),
                content: vec![UserContent::Text { text }],
                client_request_id: None,
                parent_tool_call_id: None,
            },
            provider_thread_id: self.ptid(),
        }]
    }

    fn on_agent_chunk(&mut self, chunk: &ContentChunk) -> Translated {
        let text = content_text(&chunk.content);
        if text.is_empty() {
            return Vec::new();
        }
        let id = self.ensure_assistant_item();
        vec![ProviderEvent::ItemAgentMessageDelta {
            item_id: id,
            delta: text,
            provider_thread_id: self.ptid(),
            parent_tool_call_id: None,
        }]
    }

    fn on_thought_chunk(&mut self, chunk: &ContentChunk) -> Translated {
        let text = content_text(&chunk.content);
        if text.is_empty() {
            return Vec::new();
        }
        let id = self.ensure_thinking_item(chunk);
        vec![ProviderEvent::ItemReasoningTextDelta {
            item_id: id,
            delta: text,
            provider_thread_id: self.ptid(),
            parent_tool_call_id: None,
        }]
    }

    /// The item id of the assistant message currently streaming.
    ///
    /// A new message begins once the previous one was flushed — at a turn's end
    /// or before a tool call. Those are the only boundaries ACP offers on this
    /// protocol version, whose chunks carry no message id.
    fn ensure_assistant_item(&mut self) -> String {
        if let Some(id) = &self.assistant_id {
            return id.clone();
        }
        self.assistant_seq += 1;
        let id = format!("assistant-{}", self.assistant_seq);
        self.assistant_id = Some(id.clone());
        id
    }

    /// A thinking block's item id, keyed by ACP's content index.
    fn ensure_thinking_item(&mut self, chunk: &ContentChunk) -> String {
        let index = chunk
            .meta
            .as_ref()
            .and_then(|meta| meta.get("contentIndex"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        if let Some(id) = self.thinking_ids.get(&index) {
            return id.clone();
        }
        self.reasoning_seq += 1;
        let id = format!("reasoning-{}", self.reasoning_seq);
        self.thinking_ids.insert(index, id.clone());
        id
    }

    /// Closes the streaming assistant message, if one is open.
    ///
    /// ACP has no "message completed" update on this protocol version, so the
    /// item is completed when something else begins. Without this the
    /// projection would hold an item open forever.
    ///
    /// The text is empty because the deltas already carried it and the
    /// projection joins by item id; restating it would duplicate the content in
    /// the log.
    fn flush_assistant(&mut self) -> Translated {
        let Some(id) = self.assistant_id.take() else {
            return Vec::new();
        };
        vec![ProviderEvent::ItemCompleted {
            item: ThreadEventItem::AgentMessage {
                id,
                text: String::new(),
                presentation: None,
                parent_tool_call_id: None,
            },
            provider_thread_id: self.ptid(),
        }]
    }

    // --- tools ------------------------------------------------------------

    fn on_tool_call(&mut self, call: &ToolCall) -> Translated {
        // A tool call ends the assistant's current message: the model stopped
        // to call something, so whatever streams next is a new message.
        let mut events = self.flush_assistant();
        let item = item_from_tool_call(call, self.ctx.cwd.as_deref());
        self.tools.insert(
            call.tool_call_id.0.to_string(),
            ToolItem { item: item.clone() },
        );
        events.push(ProviderEvent::ItemStarted {
            item,
            provider_thread_id: self.ptid(),
        });
        events
    }

    fn on_tool_update(&mut self, patch: &ToolCallUpdate) -> Translated {
        let key = patch.tool_call_id.0.to_string();
        let Some(tool) = self.tools.get_mut(&key) else {
            // An update for a call loom never saw started. ACP allows a client
            // to learn of a call through an update alone, but an item loom
            // cannot describe is worse than none: report nothing rather than
            // fabricate a start.
            return Vec::new();
        };

        // A patch's `content` replaces the collection, so the item is rebuilt
        // from the merged state rather than patched in place.
        if patch.fields.content.is_some() {
            tool.item = rebuild_tool_item(tool, patch);
        }

        let status = patch
            .fields
            .status
            .map(item_status)
            .or_else(|| item_status_of(&tool.item))
            .unwrap_or(ItemStatus::Pending);

        // The item's own status has to move with the update, because the
        // projection reads the item rather than the event.
        set_item_status(&mut tool.item, status);

        if matches!(status, ItemStatus::Pending) {
            return vec![ProviderEvent::ItemToolCallProgress {
                item_id: key,
                message: patch.fields.title.clone(),
                provider_thread_id: self.ptid(),
                parent_tool_call_id: None,
            }];
        }

        // Terminal: hand back the merged item and forget it, so a stray later
        // update cannot reopen a finished call.
        let item = self.tools.remove(&key).expect("just looked it up").item;
        vec![ProviderEvent::ItemCompleted {
            item,
            provider_thread_id: self.ptid(),
        }]
    }

    // --- plan, usage, name ------------------------------------------------

    fn on_plan(&mut self, plan: &Plan) -> Translated {
        self.plan_seq += 1;
        let steps = plan
            .entries
            .iter()
            .map(|entry| PlanStep {
                step: entry.content.clone(),
                status: Some(plan_step_status(&entry.status)),
            })
            .collect();
        vec![ProviderEvent::TurnPlanUpdated {
            provider_thread_id: self.ptid(),
            plan: steps,
            // ACP sends the whole plan each time rather than a delta, so there
            // is no explanation to carry and the steps are authoritative.
            explanation: None,
        }]
    }

    /// ACP reports *context window* usage (`used` of `size`), not a token
    /// breakdown, so it maps to the context-window event rather than the
    /// token-usage one. Reporting it as token usage would state counts the
    /// provider never sent.
    fn on_usage(&mut self, usage: &UsageUpdate) -> Translated {
        vec![ProviderEvent::ThreadContextWindowUsageUpdated {
            provider_thread_id: self.ptid(),
            context_window_usage: loom_domain::ContextWindowUsage {
                used_tokens: Some(usage.used),
                model_context_window: Some(usage.size),
                estimated: false,
            },
        }]
    }

    fn on_session_info(&mut self, info: &SessionInfoUpdate) -> Translated {
        // Only the title is a contract fact. ACP also carries a timestamp here,
        // which loom already owns. `title` is a patch field: `Undefined` means
        // "unchanged" and `Null` means "cleared", and neither is a name to
        // report, so only a concrete value emits.
        let Some(title) = info.title.value() else {
            return Vec::new();
        };
        vec![ProviderEvent::ThreadNameUpdated {
            provider_thread_id: self.ptid(),
            thread_name: title.clone(),
        }]
    }
}

// --- tool mapping ---------------------------------------------------------

/// Builds a contract item from an ACP tool call.
///
/// The variant comes from the data present, not from `kind` alone: rule 1 in
/// the module docs. When a variant's required data is absent the generic tool
/// item is used, which is the accurate description rather than a fallback.
fn item_from_tool_call(call: &ToolCall, session_cwd: Option<&str>) -> ThreadEventItem {
    let id = call.tool_call_id.0.to_string();
    let args = call.raw_input.as_ref();

    match call.kind {
        ToolKind::Execute => {
            if let Some(command) = args.and_then(|a| a.get("command")).and_then(Value::as_str) {
                let cwd = args
                    .and_then(|a| a.get("cwd"))
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .or_else(|| session_cwd.map(str::to_owned))
                    .unwrap_or_default();
                return command_execution(id, command, cwd);
            }
        }
        ToolKind::Edit | ToolKind::Delete | ToolKind::Move => {
            let changes = changes_from_content(&call.content);
            if !changes.is_empty() {
                return ThreadEventItem::FileChange {
                    id,
                    changes,
                    status: ItemStatus::Pending,
                    approval_status: None,
                    presentation: None,
                    parent_tool_call_id: None,
                };
            }
        }
        ToolKind::Read => {
            if let Some(path) = read_path(call) {
                return ThreadEventItem::FileRead {
                    id,
                    path,
                    cmd: None,
                    status: ItemStatus::Pending,
                    presentation: None,
                    parent_tool_call_id: None,
                };
            }
        }
        ToolKind::Search => {
            if let Some(query) = args.and_then(|a| a.get("query")).and_then(Value::as_str) {
                return ThreadEventItem::Search {
                    id,
                    mode: SearchMode::Content,
                    query: query.to_owned(),
                    path: None,
                    cmd: None,
                    status: ItemStatus::Pending,
                    presentation: None,
                    parent_tool_call_id: None,
                };
            }
        }
        ToolKind::Fetch => {
            if let Some(url) = args.and_then(|a| a.get("url")).and_then(Value::as_str) {
                return ThreadEventItem::WebFetch {
                    id,
                    url: url.to_owned(),
                    prompt: None,
                    pattern: None,
                    result_text: None,
                    presentation: None,
                    parent_tool_call_id: None,
                };
            }
        }
        ToolKind::Think => {
            return ThreadEventItem::Reasoning {
                id,
                summary: Vec::new(),
                content: Vec::new(),
                presentation: None,
                parent_tool_call_id: None,
            };
        }
        // A mode change is not work, so it opens no specific item; the generic
        // tool item still records that the call happened.
        // `ToolKind` is `#[non_exhaustive]`: an unknown kind has no specific
        // variant to map to, which is exactly what the generic item is for.
        _ => {}
    }

    generic_tool_call(call, id)
}

fn command_execution(id: String, command: &str, cwd: String) -> ThreadEventItem {
    ThreadEventItem::CommandExecution {
        id,
        command: command.to_owned(),
        cwd,
        status: ItemStatus::Pending,
        approval_status: None,
        aggregated_output: None,
        exit_code: None,
        duration_ms: None,
        presentation: None,
        parent_tool_call_id: None,
    }
}

/// The generic tool item, which needs only what ACP always supplies.
fn generic_tool_call(call: &ToolCall, id: String) -> ThreadEventItem {
    ThreadEventItem::ToolCall {
        id,
        server: None,
        tool: tool_kind_name(call.kind).to_string(),
        arguments: call
            .raw_input
            .as_ref()
            .and_then(Value::as_object)
            .map(|object| {
                object
                    .iter()
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect::<BTreeMap<_, _>>()
            }),
        status: ItemStatus::Pending,
        result: call.raw_output.clone(),
        error: None,
        duration_ms: None,
        presentation: None,
        parent_tool_call_id: None,
    }
}

/// A stable lower-case name for a tool kind, used as `ToolCall.tool`.
fn tool_kind_name(kind: ToolKind) -> &'static str {
    match kind {
        ToolKind::Read => "read",
        ToolKind::Edit => "edit",
        ToolKind::Delete => "delete",
        ToolKind::Move => "move",
        ToolKind::Search => "search",
        ToolKind::Execute => "execute",
        ToolKind::Think => "think",
        ToolKind::Fetch => "fetch",
        ToolKind::SwitchMode => "switch_mode",
        ToolKind::Other => "other",
        _ => "other",
    }
}

/// File changes from ACP's diff content.
///
/// Richer than the Pi path, which infers the change kind from argument key
/// names: ACP supplies each file's path and its before/after text, so the kind
/// is derived from fact. `content` is an array, so a multi-file edit becomes
/// one item with several entries.
fn changes_from_content(content: &[ToolCallContent]) -> Vec<loom_domain::FileChange> {
    content
        .iter()
        .filter_map(|entry| match entry {
            ToolCallContent::Diff(diff) => {
                // An absent `old_text` means the file did not exist before.
                let kind = if diff.old_text.is_none() {
                    loom_domain::FileChangeKind::Add
                } else {
                    loom_domain::FileChangeKind::Update
                };
                Some(loom_domain::FileChange {
                    path: diff.path.to_string_lossy().into_owned(),
                    kind,
                    move_path: None,
                    diff: Some(diff.new_text.clone()),
                })
            }
            _ => None,
        })
        .collect()
}

/// A path for a read, from ACP's locations or the arguments.
fn read_path(call: &ToolCall) -> Option<String> {
    if let Some(location) = call.locations.first() {
        return Some(location.path.to_string_lossy().into_owned());
    }
    call.raw_input
        .as_ref()
        .and_then(|args| args.get("path").or_else(|| args.get("file_path")))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// Rebuilds a tool item when a patch replaces its content.
///
/// A content replacement can change which variant the item is, so the item is
/// rebuilt through the same data-driven rules rather than patched in place.
fn rebuild_tool_item(tool: &ToolItem, patch: &ToolCallUpdate) -> ThreadEventItem {
    let Some(content) = &patch.fields.content else {
        return tool.item.clone();
    };
    let changes = changes_from_content(content);
    if changes.is_empty() {
        return tool.item.clone();
    }
    ThreadEventItem::FileChange {
        id: tool.item.id().to_string(),
        changes,
        status: item_status_of(&tool.item).unwrap_or(ItemStatus::Pending),
        approval_status: None,
        presentation: None,
        parent_tool_call_id: None,
    }
}

/// ACP's tool status in the contract's terms.
fn item_status(status: ToolCallStatus) -> ItemStatus {
    match status {
        ToolCallStatus::Pending | ToolCallStatus::InProgress => ItemStatus::Pending,
        ToolCallStatus::Completed => ItemStatus::Completed,
        ToolCallStatus::Failed => ItemStatus::Failed,
        // `#[non_exhaustive]`: an unknown status is not a finish.
        _ => ItemStatus::Pending,
    }
}

/// The status already recorded on an item.
fn item_status_of(item: &ThreadEventItem) -> Option<ItemStatus> {
    use ThreadEventItem as I;
    match item {
        I::CommandExecution { status, .. }
        | I::FileChange { status, .. }
        | I::FileRead { status, .. }
        | I::Search { status, .. }
        | I::ToolCall { status, .. }
        | I::ImageGeneration { status, .. }
        | I::PlanSteps { status, .. }
        | I::Delegation { status, .. }
        | I::Extension { status, .. } => Some(*status),
        _ => None,
    }
}

/// Moves an item's own status, since the projection reads that rather than the
/// event that carried it.
fn set_item_status(item: &mut ThreadEventItem, status: ItemStatus) {
    use ThreadEventItem as I;
    let slot = match item {
        I::CommandExecution { status: s, .. }
        | I::FileChange { status: s, .. }
        | I::FileRead { status: s, .. }
        | I::Search { status: s, .. }
        | I::ToolCall { status: s, .. }
        | I::ImageGeneration { status: s, .. }
        | I::PlanSteps { status: s, .. }
        | I::Delegation { status: s, .. }
        | I::Extension { status: s, .. } => s,
        _ => return,
    };
    *slot = status;
}

/// ACP's plan step status in the contract's terms.
fn plan_step_status(status: &PlanEntryStatus) -> PlanStepStatus {
    match status {
        PlanEntryStatus::Pending => PlanStepStatus::Pending,
        PlanEntryStatus::InProgress => PlanStepStatus::Active,
        PlanEntryStatus::Completed => PlanStepStatus::Completed,
        // `#[non_exhaustive]`: an unknown status is "not started yet" rather
        // than a guess at progress.
        _ => PlanStepStatus::Pending,
    }
}

/// The text of a content block, when it has one.
///
/// Non-text content has no place in the contract's text deltas; those events
/// carry only a string.
fn content_text(block: &ContentBlock) -> String {
    match block {
        ContentBlock::Text(text) => text.text.clone(),
        _ => String::new(),
    }
}

pub mod session;

#[cfg(test)]
mod tests;
