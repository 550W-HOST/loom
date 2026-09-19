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

use std::collections::{BTreeMap, HashMap, HashSet};

use agent_client_protocol_schema::v1::{
    ContentBlock, ContentChunk, Plan, PlanEntryStatus, SessionInfoUpdate, SessionUpdate,
    StopReason, ToolCall, ToolCallContent, ToolCallStatus, ToolCallUpdate, ToolKind, UsageUpdate,
};
use agent_client_protocol_schema::v2;
use loom_domain::{
    ItemStatus, PlanStep, PlanStepStatus, ProviderEvent, SearchMode, ThreadEventItem, TurnError,
    TurnStatus, UserContent,
};
use serde_json::Value;

/// What a translator is built from: the parts of a run every event needs.
pub struct RunContext {
    /// loom's thread id, used only when no agent session is known.
    pub thread_id: loom_domain::ThreadId,
    /// The workspace the provider runs in, used as a tool item's `cwd`.
    pub cwd: Option<String>,
    /// The agent's session id, when the dispatch already knows one.
    pub provider_session_id: Option<String>,
}

/// One run's ACP frames, translated into contract bodies.
///
/// Public so tests (and the transport) can drive the translation directly
/// instead of through a live agent.
pub struct AcpTranslator {
    ctx: RunContext,
    /// The agent's session id, learned from `session/new` or supplied by the
    /// dispatch when this run resumes one.
    provider_session_id: Option<String>,
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
    /// v2 message patches are keyed by the agent-supplied message id. Keeping
    /// the accumulated text here lets a repeated full patch become only the
    /// newly appended suffix in loom's delta-only contract.
    v2_agent_messages: HashMap<String, V2MessageState>,
    v2_current_agent_message: Option<String>,
    v2_thought_messages: HashMap<String, V2MessageState>,
    /// v2 user upserts are complete snapshots, so only the first non-empty
    /// snapshot becomes a timeline item.
    v2_user_messages: HashSet<String>,
    /// v2 user chunks are deltas but loom has no user-message delta event. Keep
    /// them until the next full user upsert or agent update so the item is not
    /// truncated to its first chunk.
    v2_pending_user_messages: BTreeMap<String, String>,
    /// v2 tool calls are upserts rather than the v1 start/update pair.
    v2_tools: HashMap<String, V2ToolState>,
    v2_finished_tools: HashSet<String>,
    /// Agent-owned terminals have their own lifecycle and output stream in v2.
    v2_terminals: HashMap<String, V2TerminalState>,
    v2_finished_terminals: HashSet<String>,
}

/// A tool call's accumulated state.
struct ToolItem {
    item: ThreadEventItem,
}

struct V2MessageState {
    item_id: String,
    text: String,
}

struct V2ToolState {
    started: bool,
    title: Option<String>,
    kind: v2::ToolKind,
    status: v2::ToolCallStatus,
    content: Vec<v2::ToolCallContent>,
    locations: Vec<v2::ToolCallLocation>,
    raw_input: Option<Value>,
    raw_output: Option<Value>,
    /// What pi-acp streamed through `_meta.terminal_output`, in arrival order.
    terminal_output: String,
    /// What pi-acp reported through `_meta.terminal_exit`.
    terminal_exit_code: Option<i64>,
}

struct V2TerminalState {
    item_id: String,
    command: Option<String>,
    cwd: Option<String>,
    output: String,
    exit: Option<v2::TerminalExitStatus>,
    started: bool,
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
            provider_session_id: ctx.provider_session_id.clone(),
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
            v2_agent_messages: HashMap::new(),
            v2_current_agent_message: None,
            v2_thought_messages: HashMap::new(),
            v2_user_messages: HashSet::new(),
            v2_pending_user_messages: BTreeMap::new(),
            v2_tools: HashMap::new(),
            v2_finished_tools: HashSet::new(),
            v2_terminals: HashMap::new(),
            v2_finished_terminals: HashSet::new(),
        }
    }

    /// The provider thread id reported on every event.
    ///
    /// This is the *agent's* identifier for the conversation, not loom's
    /// thread id: it is what a resumed session is keyed by, and what the
    /// control plane stores so a later turn can continue this conversation.
    ///
    /// Before the agent names a session there is nothing true to report, so the
    /// thread id stands in. Callers avoid emitting in that window by deferring
    /// updates until the session is known — see `UpdateSink::on_notification`.
    fn ptid(&self) -> String {
        self.provider_session_id
            .clone()
            .unwrap_or_else(|| self.ctx.thread_id.to_string())
    }

    /// Records the agent's session id, once `session/new` (or a resume) named
    /// it.
    pub fn set_provider_session_id(&mut self, session_id: impl Into<String>) {
        let session_id = session_id.into();
        if !session_id.is_empty() {
            self.provider_session_id = Some(session_id);
        }
    }

    /// The session id currently used for provider events, when the agent has
    /// named the session.
    pub fn provider_session_id(&self) -> Option<&str> {
        self.provider_session_id.as_deref()
    }

    /// Whether `thread/identity` has been emitted, which is what makes it safe
    /// to report events.
    ///
    /// Not the same question as "is the session id known": a resumed run knows
    /// the id from the start but has not yet stated it, and events reported in
    /// that window would precede the identity that explains them.
    pub fn has_identity(&self) -> bool {
        self.identified
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

    /// One ACP v2 session update.
    ///
    /// v2 adds full message patches, append-only tool content, terminal
    /// updates and an explicit foreground state. Those differences stay here;
    /// callers still receive the same loom provider events as the v1 path.
    pub fn on_v2_session_update(&mut self, update: &v2::SessionUpdate) -> Translated {
        let mut events = match update {
            v2::SessionUpdate::UserMessageChunk(_) | v2::SessionUpdate::UserMessage(_) => {
                Vec::new()
            }
            _ => self.flush_v2_user_messages(),
        };
        events.extend(match update {
            v2::SessionUpdate::UserMessageChunk(chunk) => self.on_v2_user_chunk(chunk),
            v2::SessionUpdate::UserMessage(message) => self.on_v2_user_message(message),
            v2::SessionUpdate::AgentMessageChunk(chunk) => self.on_v2_agent_chunk(chunk),
            v2::SessionUpdate::AgentMessage(message) => self.on_v2_agent_message(message),
            v2::SessionUpdate::AgentThoughtChunk(chunk) => self.on_v2_thought_chunk(chunk),
            v2::SessionUpdate::AgentThought(message) => self.on_v2_thought_message(message),
            v2::SessionUpdate::StateUpdate(state) => self.on_v2_state_update(state),
            v2::SessionUpdate::ToolCallContentChunk(chunk) => self.on_v2_tool_content_chunk(chunk),
            v2::SessionUpdate::ToolCallUpdate(patch) => self.on_v2_tool_update(patch),
            v2::SessionUpdate::TerminalUpdate(update) => self.on_v2_terminal_update(update),
            v2::SessionUpdate::TerminalOutputChunk(chunk) => {
                self.on_v2_terminal_output_chunk(chunk)
            }
            v2::SessionUpdate::PlanUpdate(plan) => self.on_v2_plan(plan),
            v2::SessionUpdate::SessionInfoUpdate(info) => self.on_v2_session_info(info),
            v2::SessionUpdate::UsageUpdate(usage) => self.on_v2_usage(usage),
            v2::SessionUpdate::AvailableCommandsUpdate(_)
            | v2::SessionUpdate::ConfigOptionUpdate(_)
            | v2::SessionUpdate::Other(_) => Vec::new(),
            // `PlanRemoved` is behind an optional schema feature and is not
            // enabled by loom. Keep the wildcard for future v2 additions.
            _ => Vec::new(),
        });
        events
    }

    /// The turn finished, in the protocol's terms.
    ///
    /// Both versions funnel here: v1 reports `stop_reason` on the
    /// `session/prompt` response, v2 through `StateUpdate::Idle`. Normalizing
    /// at the call site keeps the mapping in one place.
    pub fn on_stop_reason(&mut self, reason: StopReason) -> Translated {
        self.finish_stop_reason(reason, false)
    }

    /// The v2 completion signal is an idle state update rather than a prompt
    /// response. It uses the same terminal-event mapping as v1, while also
    /// closing any message item keyed by a v2 `messageId`.
    pub fn on_v2_stop_reason(&mut self, reason: v2::StopReason) -> Translated {
        let reason = match reason {
            v2::StopReason::EndTurn => StopReason::EndTurn,
            v2::StopReason::MaxTokens => StopReason::MaxTokens,
            v2::StopReason::MaxTurnRequests => StopReason::MaxTurnRequests,
            v2::StopReason::Refusal => StopReason::Refusal,
            v2::StopReason::Cancelled => StopReason::Cancelled,
            // A future stop reason is still a stop. Completing the turn keeps
            // the control plane from being left in `working`.
            _ => StopReason::EndTurn,
        };
        self.finish_stop_reason(reason, true)
    }

    fn finish_stop_reason(&mut self, reason: StopReason, include_v2_messages: bool) -> Translated {
        if !self.turn_open
            && self.assistant_id.is_none()
            && (!include_v2_messages || self.v2_current_agent_message.is_none())
        {
            return Vec::new();
        }
        let mut events = self.flush_assistant();
        if include_v2_messages {
            events.extend(self.flush_v2_assistant());
        }
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

    fn on_v2_user_chunk(&mut self, chunk: &v2::ContentChunk) -> Translated {
        let text = v2_content_text(&chunk.content);
        if text.is_empty() {
            return Vec::new();
        }
        let message_id = chunk.message_id.to_string();
        self.v2_pending_user_messages
            .entry(message_id)
            .or_default()
            .push_str(&text);
        Vec::new()
    }

    fn on_v2_user_message(&mut self, message: &v2::UserMessage) -> Translated {
        let Some(content) = message.content.value() else {
            return Vec::new();
        };
        let text = v2_content_texts(content);
        if text.is_empty() {
            return Vec::new();
        }
        let message_id = message.message_id.to_string();
        if !self.v2_user_messages.insert(message_id.clone()) {
            return Vec::new();
        }
        self.v2_pending_user_messages.remove(&message_id);
        vec![ProviderEvent::ItemStarted {
            item: ThreadEventItem::UserMessage {
                id: format!("user-v2-{message_id}"),
                content: vec![UserContent::Text { text }],
                client_request_id: None,
                parent_tool_call_id: None,
            },
            provider_thread_id: self.ptid(),
        }]
    }

    fn on_v2_agent_chunk(&mut self, chunk: &v2::ContentChunk) -> Translated {
        let text = v2_content_text(&chunk.content);
        if text.is_empty() {
            return Vec::new();
        }
        self.append_v2_agent_message(chunk.message_id.to_string(), text)
    }

    fn on_v2_agent_message(&mut self, message: &v2::AgentMessage) -> Translated {
        let Some(content) = message.content.value() else {
            // `null` clears the provider's snapshot. loom's event contract has
            // no message-reset operation, so there is no truthful delta to log.
            if message.content.is_null() {
                if let Some(state) = self
                    .v2_agent_messages
                    .get_mut(&message.message_id.to_string())
                {
                    state.text.clear();
                }
            }
            return Vec::new();
        };
        self.patch_v2_agent_message(message.message_id.to_string(), v2_content_texts(content))
    }

    fn flush_v2_user_messages(&mut self) -> Translated {
        let pending = std::mem::take(&mut self.v2_pending_user_messages);
        pending
            .into_iter()
            .map(|(message_id, text)| {
                self.v2_user_messages.insert(message_id.clone());
                ProviderEvent::ItemStarted {
                    item: ThreadEventItem::UserMessage {
                        id: format!("user-v2-{message_id}"),
                        content: vec![UserContent::Text { text }],
                        client_request_id: None,
                        parent_tool_call_id: None,
                    },
                    provider_thread_id: self.ptid(),
                }
            })
            .collect()
    }

    fn on_v2_thought_chunk(&mut self, chunk: &v2::ContentChunk) -> Translated {
        let text = v2_content_text(&chunk.content);
        if text.is_empty() {
            return Vec::new();
        }
        self.append_v2_thought(chunk.message_id.to_string(), text)
    }

    fn on_v2_thought_message(&mut self, message: &v2::AgentThought) -> Translated {
        let Some(content) = message.content.value() else {
            if message.content.is_null() {
                if let Some(state) = self
                    .v2_thought_messages
                    .get_mut(&message.message_id.to_string())
                {
                    state.text.clear();
                }
            }
            return Vec::new();
        };
        self.patch_v2_thought(message.message_id.to_string(), v2_content_texts(content))
    }

    fn append_v2_agent_message(&mut self, message_id: String, text: String) -> Translated {
        let mut events = Vec::new();
        if self.v2_current_agent_message.as_deref() != Some(message_id.as_str()) {
            events.extend(self.flush_v2_assistant());
            self.v2_current_agent_message = Some(message_id.clone());
        }
        let state = self
            .v2_agent_messages
            .entry(message_id.clone())
            .or_insert_with(|| V2MessageState {
                item_id: format!("assistant-v2-{message_id}"),
                text: String::new(),
            });
        state.text.push_str(&text);
        events.push(ProviderEvent::ItemAgentMessageDelta {
            item_id: state.item_id.clone(),
            delta: text,
            provider_thread_id: self.ptid(),
            parent_tool_call_id: None,
        });
        events
    }

    fn patch_v2_agent_message(&mut self, message_id: String, text: String) -> Translated {
        let mut events = Vec::new();
        if self.v2_current_agent_message.as_deref() != Some(message_id.as_str()) {
            events.extend(self.flush_v2_assistant());
            self.v2_current_agent_message = Some(message_id.clone());
        }
        let item_id = format!("assistant-v2-{message_id}");
        let state = self
            .v2_agent_messages
            .entry(message_id)
            .or_insert_with(|| V2MessageState {
                item_id,
                text: String::new(),
            });
        let delta = patch_message_text(&mut state.text, &text);
        if !delta.is_empty() {
            events.push(ProviderEvent::ItemAgentMessageDelta {
                item_id: state.item_id.clone(),
                delta,
                provider_thread_id: self.ptid(),
                parent_tool_call_id: None,
            });
        }
        events
    }

    fn append_v2_thought(&mut self, message_id: String, text: String) -> Translated {
        let item_id = format!("reasoning-v2-{message_id}");
        let state = self
            .v2_thought_messages
            .entry(message_id)
            .or_insert_with(|| V2MessageState {
                item_id,
                text: String::new(),
            });
        state.text.push_str(&text);
        vec![ProviderEvent::ItemReasoningTextDelta {
            item_id: state.item_id.clone(),
            delta: text,
            provider_thread_id: self.ptid(),
            parent_tool_call_id: None,
        }]
    }

    fn patch_v2_thought(&mut self, message_id: String, text: String) -> Translated {
        let item_id = format!("reasoning-v2-{message_id}");
        let state = self
            .v2_thought_messages
            .entry(message_id)
            .or_insert_with(|| V2MessageState {
                item_id,
                text: String::new(),
            });
        let delta = patch_message_text(&mut state.text, &text);
        if delta.is_empty() {
            return Vec::new();
        }
        vec![ProviderEvent::ItemReasoningTextDelta {
            item_id: state.item_id.clone(),
            delta,
            provider_thread_id: self.ptid(),
            parent_tool_call_id: None,
        }]
    }

    /// Completes the currently streaming v2 assistant message. A repeated full
    /// patch never reaches this as a second item because the message id owns a
    /// single stable item id.
    fn flush_v2_assistant(&mut self) -> Translated {
        let Some(message_id) = self.v2_current_agent_message.take() else {
            return Vec::new();
        };
        let Some(state) = self.v2_agent_messages.get(&message_id) else {
            return Vec::new();
        };
        vec![ProviderEvent::ItemCompleted {
            item: ThreadEventItem::AgentMessage {
                id: state.item_id.clone(),
                text: String::new(),
                presentation: None,
                parent_tool_call_id: None,
            },
            provider_thread_id: self.ptid(),
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

    fn on_v2_state_update(&mut self, state: &v2::StateUpdate) -> Translated {
        match state {
            v2::StateUpdate::Idle(idle) => {
                self.on_v2_stop_reason(idle.stop_reason.clone().unwrap_or(v2::StopReason::EndTurn))
            }
            v2::StateUpdate::Running(_)
            | v2::StateUpdate::RequiresAction(_)
            | v2::StateUpdate::Other(_) => Vec::new(),
            _ => Vec::new(),
        }
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

    fn on_v2_tool_content_chunk(&mut self, chunk: &v2::ToolCallContentChunk) -> Translated {
        let key = chunk.tool_call_id.0.to_string();
        if self.v2_finished_tools.contains(&key) {
            return Vec::new();
        }
        let mut events = self.flush_v2_assistant();
        let session_cwd = self.ctx.cwd.clone();
        let (started, item) = {
            let state = self
                .v2_tools
                .entry(key.clone())
                .or_insert_with(v2_tool_state);
            state.content.push(chunk.content.clone());
            let started = !state.started;
            state.started = true;
            let item = v2_item_from_tool_state(state, &key, session_cwd.as_deref());
            (started, item)
        };
        if started {
            events.push(ProviderEvent::ItemStarted {
                item,
                provider_thread_id: self.ptid(),
            });
        }
        events
    }

    fn on_v2_tool_update(&mut self, patch: &v2::ToolCallUpdate) -> Translated {
        let key = patch.tool_call_id.0.to_string();
        if self.v2_finished_tools.contains(&key) {
            return Vec::new();
        }
        let mut events = self.flush_v2_assistant();
        let session_cwd = self.ctx.cwd.clone();
        let provider_thread_id = self.ptid();
        let (started, status, title) = {
            let state = self
                .v2_tools
                .entry(key.clone())
                .or_insert_with(v2_tool_state);
            if !patch.title.is_undefined() {
                state.title = patch.title.value().cloned();
            }
            if !patch.kind.is_undefined() {
                state.kind = patch.kind.value().cloned().unwrap_or(v2::ToolKind::Other);
            }
            if !patch.status.is_undefined() {
                state.status = patch
                    .status
                    .value()
                    .cloned()
                    .unwrap_or(v2::ToolCallStatus::Pending);
            }
            if !patch.content.is_undefined() {
                state.content = patch.content.value().cloned().unwrap_or_default();
            }
            if !patch.locations.is_undefined() {
                state.locations = patch.locations.value().cloned().unwrap_or_default();
            }
            if !patch.raw_input.is_undefined() {
                state.raw_input = patch.raw_input.value().cloned();
            }
            if !patch.raw_output.is_undefined() {
                state.raw_output = patch.raw_output.value().cloned();
            }
            if !patch.meta.is_undefined() {
                if let Some(meta) = patch.meta.value() {
                    apply_terminal_meta(state, &key, meta);
                }
            }
            let started = !state.started;
            state.started = true;
            let status = item_status_v2(&state.status);
            let title = state.title.clone();
            if started {
                let item = v2_item_from_tool_state(state, &key, session_cwd.as_deref());
                events.push(ProviderEvent::ItemStarted {
                    item,
                    provider_thread_id: provider_thread_id.clone(),
                });
            }
            (started, status, title)
        };

        if !matches!(status, ItemStatus::Pending) {
            let state = self
                .v2_tools
                .remove(&key)
                .expect("v2 tool state was inserted above");
            self.v2_finished_tools.insert(key.clone());
            events.push(ProviderEvent::ItemCompleted {
                item: v2_item_from_tool_state(&state, &key, session_cwd.as_deref()),
                provider_thread_id: provider_thread_id.clone(),
            });
        } else if !started || title.is_some() {
            events.push(ProviderEvent::ItemToolCallProgress {
                item_id: key,
                message: title,
                provider_thread_id,
                parent_tool_call_id: None,
            });
        }
        events
    }

    fn on_v2_terminal_update(&mut self, update: &v2::TerminalUpdate) -> Translated {
        let key = update.terminal_id.0.to_string();
        if self.v2_finished_terminals.contains(&key) {
            return Vec::new();
        }
        let mut events = self.flush_v2_assistant();
        let fallback_cwd = self.ctx.cwd.clone().unwrap_or_default();
        let (started, output_reset, status) = {
            let state = self
                .v2_terminals
                .entry(key.clone())
                .or_insert_with(|| V2TerminalState {
                    item_id: format!("terminal-v2-{key}"),
                    command: None,
                    cwd: None,
                    output: String::new(),
                    exit: None,
                    started: false,
                });
            if !update.command.is_undefined() {
                state.command = update.command.value().cloned();
            }
            if !update.cwd.is_undefined() {
                state.cwd = update
                    .cwd
                    .value()
                    .map(|cwd| cwd.0.to_string_lossy().into_owned());
            }
            let mut output_reset = None;
            if update.output.is_null() {
                state.output.clear();
                output_reset = Some(String::new());
            } else if let Some(output) = update.output.value() {
                if let Some(decoded) = decode_terminal_data(&output.data) {
                    state.output = decoded.clone();
                    output_reset = Some(decoded);
                }
            }
            if !update.exit_status.is_undefined() {
                state.exit = update.exit_status.value().cloned();
            }
            let started = state.command.is_some() && !state.started;
            if started {
                state.started = true;
            }
            let status = terminal_item_status(state.exit.as_ref());
            (started, output_reset, status)
        };

        let state = self
            .v2_terminals
            .get(&key)
            .expect("v2 terminal state was inserted above");
        if started {
            let item = terminal_item(state, &fallback_cwd, status);
            events.push(ProviderEvent::ItemStarted {
                item,
                provider_thread_id: self.ptid(),
            });
        }
        if let Some(output) = output_reset {
            if !started && state.started {
                events.push(ProviderEvent::ItemCommandExecutionOutputDelta {
                    item_id: state.item_id.clone(),
                    delta: output,
                    provider_thread_id: self.ptid(),
                    reset: Some(true),
                    parent_tool_call_id: None,
                });
            }
        }
        if !matches!(status, ItemStatus::Pending) && state.started {
            if let Some(state) = self.v2_terminals.remove(&key) {
                self.v2_finished_terminals.insert(key.clone());
                events.push(ProviderEvent::ItemCompleted {
                    item: terminal_item(&state, &fallback_cwd, status),
                    provider_thread_id: self.ptid(),
                });
            }
        }
        events
    }

    fn on_v2_terminal_output_chunk(&mut self, chunk: &v2::TerminalOutputChunk) -> Translated {
        let key = chunk.terminal_id.0.to_string();
        if self.v2_finished_terminals.contains(&key) {
            return Vec::new();
        }
        let Some(delta) = decode_terminal_data(&chunk.data) else {
            return Vec::new();
        };
        let mut events = self.flush_v2_assistant();
        let fallback_cwd = self.ctx.cwd.clone().unwrap_or_default();
        let provider_thread_id = self.ptid();
        let can_emit = {
            let state = self
                .v2_terminals
                .entry(key.clone())
                .or_insert_with(|| V2TerminalState {
                    item_id: format!("terminal-v2-{key}"),
                    command: None,
                    cwd: None,
                    output: String::new(),
                    exit: None,
                    started: false,
                });
            state.output.push_str(&delta);
            let started_now = state.command.is_some() && !state.started;
            if started_now {
                state.started = true;
                let item = terminal_item(state, &fallback_cwd, ItemStatus::Pending);
                events.push(ProviderEvent::ItemStarted {
                    item,
                    provider_thread_id: provider_thread_id.clone(),
                });
            }
            state.command.is_some()
        };
        if can_emit {
            let item_id = self
                .v2_terminals
                .get(&key)
                .map(|state| state.item_id.clone())
                .expect("v2 terminal state was inserted above");
            events.push(ProviderEvent::ItemCommandExecutionOutputDelta {
                item_id,
                delta,
                provider_thread_id,
                reset: None,
                parent_tool_call_id: None,
            });
        }
        events
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

    fn on_v2_plan(&mut self, update: &v2::PlanUpdate) -> Translated {
        let v2::PlanUpdateContent::Items(plan) = &update.plan else {
            return Vec::new();
        };
        let steps = plan
            .entries
            .iter()
            .map(|entry| PlanStep {
                step: entry.content.clone(),
                status: Some(plan_step_status_v2(&entry.status)),
            })
            .collect();
        vec![ProviderEvent::TurnPlanUpdated {
            provider_thread_id: self.ptid(),
            plan: steps,
            explanation: None,
        }]
    }

    fn on_v2_usage(&mut self, usage: &v2::UsageUpdate) -> Translated {
        vec![ProviderEvent::ThreadContextWindowUsageUpdated {
            provider_thread_id: self.ptid(),
            context_window_usage: loom_domain::ContextWindowUsage {
                used_tokens: Some(usage.used),
                model_context_window: Some(usage.size),
                estimated: false,
            },
        }]
    }

    fn on_v2_session_info(&mut self, info: &v2::SessionInfoUpdate) -> Translated {
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

fn v2_tool_state() -> V2ToolState {
    V2ToolState {
        started: false,
        title: None,
        kind: v2::ToolKind::Other,
        status: v2::ToolCallStatus::Pending,
        content: Vec::new(),
        locations: Vec::new(),
        raw_input: None,
        raw_output: None,
        terminal_output: String::new(),
        terminal_exit_code: None,
    }
}

/// Applies the terminal pi-acp carries in a tool call's `_meta`.
///
/// pi-acp does not model a shell command with ACP's terminal content. It spells
/// the command in the call's `title` and streams the terminal as `_meta`:
/// `terminal_output { terminal_id, data }` per chunk, then
/// `terminal_exit { terminal_id, exit_code, signal }`. Nothing else carries the
/// command's output, so without this an `execute` call reached the timeline as a
/// tool named "execute" whose only argument was its progress line, with no
/// output and no exit code.
///
/// `exit_code` is pi-acp's own reading — `0` for a clean exit and `1` for a
/// failed one — so a command that exited 7 reports `1` here, with pi-acp's
/// `Command exited with code 7` note in the output beside it.
fn apply_terminal_meta(state: &mut V2ToolState, tool_call_id: &str, meta: &v2::Meta) {
    // pi-acp keys each entry by the tool call it belongs to; an entry naming
    // another terminal is not this call's output.
    let belongs = |entry: &Value| {
        entry
            .get("terminal_id")
            .and_then(Value::as_str)
            .is_none_or(|terminal_id| terminal_id == tool_call_id)
    };
    if let Some(entry) = meta.get("terminal_output") {
        if belongs(entry) {
            if let Some(data) = entry.get("data").and_then(Value::as_str) {
                state.terminal_output.push_str(data);
            }
        }
    }
    if let Some(entry) = meta.get("terminal_exit") {
        if belongs(entry) {
            if let Some(exit_code) = entry.get("exit_code").and_then(Value::as_i64) {
                state.terminal_exit_code = Some(exit_code);
            }
        }
    }
}

fn v2_item_from_tool_state(
    state: &V2ToolState,
    id: &str,
    session_cwd: Option<&str>,
) -> ThreadEventItem {
    let status = item_status_v2(&state.status);
    let args = state.raw_input.as_ref();
    match state.kind {
        v2::ToolKind::Execute => {
            // Where a command is spelled differs by agent. bb's own rule is the
            // raw input's `command` when the agent sends one and the tool
            // call's title otherwise, which is what pi-acp needs: it names the
            // call `title: "pwd"` and sends no raw input at all, so a command
            // read only from `raw_input` leaves the call looking like a tool
            // with no arguments.
            let command = args
                .and_then(|input| input.get("command"))
                .and_then(Value::as_str)
                .map(str::to_owned)
                .or_else(|| {
                    state
                        .title
                        .as_deref()
                        .map(str::trim)
                        .filter(|title| !title.is_empty())
                        .map(str::to_owned)
                });
            if let Some(command) = command {
                let cwd = args
                    .and_then(|input| input.get("cwd"))
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .or_else(|| session_cwd.map(str::to_owned))
                    .unwrap_or_default();
                return ThreadEventItem::CommandExecution {
                    id: id.to_owned(),
                    command,
                    cwd,
                    status,
                    approval_status: None,
                    aggregated_output: (!state.terminal_output.is_empty())
                        .then(|| state.terminal_output.clone()),
                    exit_code: state.terminal_exit_code,
                    duration_ms: None,
                    presentation: None,
                    parent_tool_call_id: None,
                };
            }
        }
        v2::ToolKind::Edit | v2::ToolKind::Delete | v2::ToolKind::Move => {
            let changes = v2_changes_from_content(&state.content);
            if !changes.is_empty() {
                return ThreadEventItem::FileChange {
                    id: id.to_owned(),
                    changes,
                    status,
                    approval_status: None,
                    presentation: None,
                    parent_tool_call_id: None,
                };
            }
        }
        v2::ToolKind::Read => {
            if let Some(path) = v2_read_path(state) {
                return ThreadEventItem::FileRead {
                    id: id.to_owned(),
                    path,
                    cmd: None,
                    status,
                    presentation: None,
                    parent_tool_call_id: None,
                };
            }
        }
        v2::ToolKind::Search => {
            if let Some(query) = args
                .and_then(|input| input.get("query"))
                .and_then(Value::as_str)
            {
                return ThreadEventItem::Search {
                    id: id.to_owned(),
                    mode: SearchMode::Content,
                    query: query.to_owned(),
                    path: None,
                    cmd: None,
                    status,
                    presentation: None,
                    parent_tool_call_id: None,
                };
            }
        }
        v2::ToolKind::Fetch => {
            if let Some(url) = args
                .and_then(|input| input.get("url"))
                .and_then(Value::as_str)
            {
                return ThreadEventItem::WebFetch {
                    id: id.to_owned(),
                    url: url.to_owned(),
                    prompt: None,
                    pattern: None,
                    result_text: None,
                    presentation: None,
                    parent_tool_call_id: None,
                };
            }
        }
        v2::ToolKind::Think => {
            return ThreadEventItem::Reasoning {
                id: id.to_owned(),
                summary: Vec::new(),
                content: Vec::new(),
                presentation: None,
                parent_tool_call_id: None,
            };
        }
        _ => {}
    }

    ThreadEventItem::ToolCall {
        id: id.to_owned(),
        server: None,
        tool: v2_tool_kind_name(&state.kind),
        arguments: state.raw_input.as_ref().and_then(|input| {
            input.as_object().map(|object| {
                object
                    .iter()
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect::<BTreeMap<_, _>>()
            })
        }),
        status,
        result: state.raw_output.clone(),
        error: None,
        duration_ms: None,
        presentation: None,
        parent_tool_call_id: None,
    }
}

fn v2_tool_kind_name(kind: &v2::ToolKind) -> String {
    match kind {
        v2::ToolKind::Read => "read",
        v2::ToolKind::Edit => "edit",
        v2::ToolKind::Delete => "delete",
        v2::ToolKind::Move => "move",
        v2::ToolKind::Search => "search",
        v2::ToolKind::Execute => "execute",
        v2::ToolKind::Think => "think",
        v2::ToolKind::Fetch => "fetch",
        v2::ToolKind::SwitchMode => "switch_mode",
        v2::ToolKind::Other => "other",
        v2::ToolKind::Unknown(name) => name.as_str(),
        _ => "other",
    }
    .to_owned()
}

fn item_status_v2(status: &v2::ToolCallStatus) -> ItemStatus {
    match status {
        v2::ToolCallStatus::Pending | v2::ToolCallStatus::InProgress => ItemStatus::Pending,
        v2::ToolCallStatus::Completed => ItemStatus::Completed,
        v2::ToolCallStatus::Failed => ItemStatus::Failed,
        _ => ItemStatus::Pending,
    }
}

fn v2_read_path(state: &V2ToolState) -> Option<String> {
    state
        .locations
        .first()
        .map(|location| location.path.0.to_string_lossy().into_owned())
        .or_else(|| {
            state
                .raw_input
                .as_ref()
                .and_then(|input| input.get("path").or_else(|| input.get("file_path")))
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
}

fn v2_changes_from_content(content: &[v2::ToolCallContent]) -> Vec<loom_domain::FileChange> {
    content
        .iter()
        .filter_map(|entry| {
            let v2::ToolCallContent::Diff(diff) = entry else {
                return None;
            };
            let patch = diff.patch.as_ref().map(|patch| patch.text.clone());
            Some(diff.changes.iter().filter_map(move |change| {
                use v2::DiffChangeOperation;
                let (path, kind, move_path) = match &change.operation {
                    DiffChangeOperation::Add(change) => (
                        change.path.0.to_string_lossy().into_owned(),
                        loom_domain::FileChangeKind::Add,
                        None,
                    ),
                    DiffChangeOperation::Delete(change) => (
                        change.path.0.to_string_lossy().into_owned(),
                        loom_domain::FileChangeKind::Delete,
                        None,
                    ),
                    DiffChangeOperation::Modify(change) => (
                        change.path.0.to_string_lossy().into_owned(),
                        loom_domain::FileChangeKind::Update,
                        None,
                    ),
                    DiffChangeOperation::Move(change) => (
                        change.old_path.0.to_string_lossy().into_owned(),
                        loom_domain::FileChangeKind::Update,
                        Some(change.path.0.to_string_lossy().into_owned()),
                    ),
                    DiffChangeOperation::Copy(change) => (
                        change.old_path.0.to_string_lossy().into_owned(),
                        loom_domain::FileChangeKind::Add,
                        Some(change.path.0.to_string_lossy().into_owned()),
                    ),
                    _ => return None,
                };
                // ACP v2 carries no before/after text, only the agent's own
                // patch, and that patch is spelled however the agent's `git
                // diff` was invoked. It is re-spelled per file here.
                let diff = patch
                    .as_deref()
                    .and_then(|text| normalize_git_patch(text, &path));
                Some(loom_domain::FileChange {
                    path,
                    kind,
                    move_path,
                    diff,
                })
            }))
        })
        .flatten()
        .collect()
}

fn plan_step_status_v2(status: &v2::PlanEntryStatus) -> PlanStepStatus {
    match status {
        v2::PlanEntryStatus::Pending => PlanStepStatus::Pending,
        v2::PlanEntryStatus::InProgress => PlanStepStatus::Active,
        v2::PlanEntryStatus::Completed => PlanStepStatus::Completed,
        _ => PlanStepStatus::Pending,
    }
}

fn terminal_item_status(exit: Option<&v2::TerminalExitStatus>) -> ItemStatus {
    let Some(exit) = exit else {
        return ItemStatus::Pending;
    };
    if exit.signal.is_some() || exit.exit_code.is_some_and(|code| code != 0) {
        ItemStatus::Failed
    } else {
        ItemStatus::Completed
    }
}

fn terminal_item(
    terminal: &V2TerminalState,
    fallback_cwd: &str,
    status: ItemStatus,
) -> ThreadEventItem {
    let exit_code = terminal
        .exit
        .as_ref()
        .and_then(|exit| exit.exit_code)
        .map(i64::from);
    ThreadEventItem::CommandExecution {
        id: terminal.item_id.clone(),
        command: terminal.command.clone().unwrap_or_default(),
        cwd: terminal
            .cwd
            .clone()
            .unwrap_or_else(|| fallback_cwd.to_owned()),
        status,
        approval_status: None,
        aggregated_output: (!terminal.output.is_empty()).then(|| terminal.output.clone()),
        exit_code,
        duration_ms: None,
        presentation: None,
        parent_tool_call_id: None,
    }
}

fn decode_terminal_data(data: &str) -> Option<String> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .decode(data)
        .ok()
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
}

fn v2_content_text(block: &v2::ContentBlock) -> String {
    match block {
        v2::ContentBlock::Text(text) => text.text.clone(),
        _ => String::new(),
    }
}

fn v2_content_texts(blocks: &[v2::ContentBlock]) -> String {
    blocks.iter().map(v2_content_text).collect()
}

/// Converts an authoritative v2 message snapshot into the suffix supported by
/// loom's append-only delta event. A non-prefix replacement cannot be expressed
/// without a reset event, so it updates the adapter's state and waits for the
/// next append rather than duplicating already-persisted text.
fn patch_message_text(current: &mut String, snapshot: &str) -> String {
    if snapshot.starts_with(current.as_str()) {
        let delta = snapshot[current.len()..].to_owned();
        current.push_str(&delta);
        delta
    } else {
        *current = snapshot.to_owned();
        String::new()
    }
}

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
                let path = diff.path.to_string_lossy().into_owned();
                // The contract's `diff` is the whole content for a file that
                // appeared or disappeared, and a patch for one that changed —
                // which is what the client draws and counts. v1 hands over the
                // two texts rather than a patch, so the patch is built here; a
                // timeline showing an edit as the file's entire new content is
                // a diff view that says nothing changed.
                let (kind, body) = match (&diff.old_text, diff.new_text.is_empty()) {
                    (None, false) => (loom_domain::FileChangeKind::Add, diff.new_text.clone()),
                    (None, true) => return None,
                    (Some(old), true) => (loom_domain::FileChangeKind::Delete, old.clone()),
                    (Some(old), false) => (
                        loom_domain::FileChangeKind::Update,
                        unified_diff(old, &diff.new_text),
                    ),
                };
                // A file that appeared or disappeared carries its content, as the
                // contract asks; only an edit is a patch, and that patch is
                // re-spelled into the one the client can draw.
                let diff = if kind == loom_domain::FileChangeKind::Update {
                    normalize_git_patch(&body, &path)
                } else {
                    Some(body)
                };
                Some(loom_domain::FileChange {
                    path,
                    kind,
                    move_path: None,
                    diff,
                })
            }
            _ => None,
        })
        .collect()
}

/// A unified diff between two versions of one file.
///
/// Only the v1 path needs this: v2 hands over a patch already. Three lines of
/// context is what a reader expects around a change, and it is what the
/// contract's own examples show. The headers come from
/// [`normalize_git_patch`], so a v1 edit and a v2 one reach the client in the
/// same shape.
fn unified_diff(old: &str, new: &str) -> String {
    similar::TextDiff::from_lines(old, new)
        .unified_diff()
        .context_radius(3)
        .to_string()
}

/// Rewrites an agent's patch into the one spelling the client can render.
///
/// ACP promises `git_patch` text but not how it is spelled. pi writes one with
/// `git diff --no-prefix`, which starts `diff --git main.rs main.rs`; the
/// client's parser rejects that header outright ("invalid git diff header"), so
/// the row reaches the screen with no file name and no lines under it. The
/// client does render the canonical form — `diff --git a/<path> b/<path>` with
/// matching `---`/`+++` — so the adapter rewrites the headers and keeps the
/// agent's own hunks, untouched.
///
/// The path is ACP's structured `changes[].operation.path`, which the protocol
/// calls authoritative, so a patch that covers several files still lands each
/// file's hunks on that file's row. Slashes are normalized the way the client's
/// own synthetic patches are, since the header is a display path rather than
/// something that will be applied.
fn normalize_git_patch(patch: &str, path: &str) -> Option<String> {
    let relative = patch_header_path(path);
    if relative.is_empty() {
        return None;
    }
    let hunks = patch_hunks(patch, &relative)?;
    Some(format!(
        "diff --git a/{relative} b/{relative}\n--- a/{relative}\n+++ b/{relative}\n{hunks}"
    ))
}

/// The hunks of `patch` that belong to `path`, with every header line dropped.
///
/// `None` when the patch carries no hunk for it — an empty diff is not one a
/// diff view can draw, and the client has its own fallback for a change that
/// arrives without a patch.
fn patch_hunks(patch: &str, path: &str) -> Option<String> {
    let lines: Vec<&str> = patch.lines().collect();
    let starts: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.starts_with("diff --git "))
        .map(|(index, _)| index)
        .collect();
    // A patch with no `diff --git` line is a single unnamed section.
    let sections: Vec<&[&str]> = if starts.is_empty() {
        vec![&lines]
    } else {
        starts
            .iter()
            .enumerate()
            .map(|(index, start)| {
                let end = starts.get(index + 1).copied().unwrap_or(lines.len());
                &lines[*start..end]
            })
            .collect()
    };
    let section = sections
        .iter()
        .find(|section| section_paths(section).iter().any(|named| named == path))
        .or_else(|| sections.first().filter(|_| sections.len() == 1))?;
    let first_hunk = section.iter().position(|line| line.starts_with("@@"))?;
    let mut hunks = section[first_hunk..].join("\n");
    hunks.push('\n');
    Some(hunks)
}

/// The file paths a patch section's headers name, in header spelling.
fn section_paths(section: &[&str]) -> Vec<String> {
    let mut paths = Vec::new();
    for line in section {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            let mut parts = rest.split_whitespace().map(unquote_patch_path);
            if let Some(old) = parts.next() {
                paths.push(old);
            }
            if let Some(new) = parts.next_back() {
                paths.push(new);
            }
            continue;
        }
        if let Some(rest) = line
            .strip_prefix("--- ")
            .or_else(|| line.strip_prefix("+++ "))
        {
            paths.push(unquote_patch_path(rest));
        }
    }
    paths
        .into_iter()
        .map(|named| patch_header_path(&named))
        .filter(|named| !named.is_empty() && named != "dev/null")
        .collect()
}

/// A patch path as a row spells it: `/`-separated, without a leading slash and
/// without git's `a/`/`b/` side prefix.
fn patch_header_path(path: &str) -> String {
    let normalized = path.replace('\\', "/");
    let trimmed = normalized.trim_start_matches('/');
    match trimmed.split_once('/') {
        Some(("a" | "b", rest)) => rest.to_owned(),
        _ => trimmed.to_owned(),
    }
}

/// A header path without git's quotes, which it adds around paths with spaces.
fn unquote_patch_path(path: &str) -> String {
    path.trim_matches('"').to_owned()
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

pub mod catalog;
pub mod permission;
pub mod session;
pub mod sessions;

#[cfg(test)]
mod tests;
