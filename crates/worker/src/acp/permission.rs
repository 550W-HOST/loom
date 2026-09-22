//! Bridging ACP `session/request_permission` to the control plane's
//! interaction entity, and the answer back.
//!
//! The ACP client must answer a permission request, and the answer is the
//! *user's*, not the worker's. So the worker holds the request open and asks the
//! control plane, which is where a UI can see and answer it. Two frames carry
//! the exchange, in opposite directions and over different transports:
//!
//! ```text
//!   agent ──session/request_permission──▶ adapter
//!                                          │  InteractionRequest (up the socket)
//!                                          ▼
//!                                      server: durable Interaction, published
//!                                        to thread:{id}; a client answers
//!                                          │  InteractionResolutionFrame
//!                                          ▼  (through the relay, to host:{id})
//!   agent ◀──RequestPermissionResponse──── broker
//! ```
//!
//! # Why the answer does not come back on the socket
//!
//! A resolution travels down through the relay to `host:{id}`, not as a reply to
//! the worker's own `InteractionRequest` frame. That is what lets the answering
//! client be an ordinary UI that never speaks the worker protocol, and what
//! makes a resolution published while the worker was reconnecting replayable to
//! it. It is the same shape dispatch already uses.
//!
//! # What this module refuses to do
//!
//! There is no default answer. The previous implementation picked the first
//! allowing option and answered with it; that is a policy no user consented to,
//! and it silently approved operations. Here a request that nobody answers is
//! **cancelled** after a bounded wait, which the agent reads as "no"; and a
//! request the control plane refuses to record is cancelled immediately rather
//! than held open where nothing can reach it. Cancellation is the only failure
//! mode, because a permission the client never granted must never look granted.

use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use std::time::Duration;

use agent_client_protocol::schema::{v1, v2};
use loom_domain::{HostPermissionMode, InteractionKind, InteractionPayload};
use loom_provider_protocol::{InteractionAnswer, InteractionRequest, PermissionDecision};
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot, Mutex};

use crate::provider::ProviderRun;

/// How long a permission request waits for a user by default.
///
/// Long enough that a human notices the dialog and answers, short enough that a
/// run blocked with no client attached does not hold its thread in `working`
/// until the run deadline. The run deadline remains the outer bound; this is the
/// tighter one for the question itself.
pub const DEFAULT_PERMISSION_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// The permission choice categories the control plane understands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PermissionChoiceKind {
    AllowOnce,
    AllowAlways,
    RejectOnce,
    RejectAlways,
    Other,
}

/// A permission option without an ACP version in its type.
#[derive(Clone, Debug, PartialEq, Eq)]
struct NormalizedPermissionOption {
    id: String,
    name: String,
    kind: PermissionChoiceKind,
}

/// The subject of a permission prompt, normalized only at the worker boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
enum PermissionSubject {
    ToolCall {
        item_id: String,
        tool: String,
        title: Option<String>,
        raw_input: Option<Value>,
        raw_output: Option<Value>,
    },
    Command {
        item_id: String,
        command: String,
        cwd: String,
        terminal_id: Option<String>,
    },
    Other {
        type_name: String,
        raw: Value,
    },
    None,
}

/// A permission request in the worker's own model.
///
/// v1 and v2 are parsed into this shape independently. The broker below only
/// waits for the control-plane answer; it never needs to know which ACP schema
/// carried the prompt.
#[derive(Clone, Debug, PartialEq, Eq)]
struct PermissionPrompt {
    session_id: String,
    request_id: String,
    title: String,
    description: Option<String>,
    subject: PermissionSubject,
    options: Vec<NormalizedPermissionOption>,
}

/// A permission request waiting for its answer.
#[derive(Clone)]
pub struct PermissionBroker {
    /// The run asking. Every frame carries its identity so the control plane can
    /// check that this host owns the run the question belongs to.
    run: ProviderRun,
    /// Where the request travels to the socket loop.
    outbound: mpsc::Sender<InteractionRequest>,
    pending: PermissionRegistry,
    timeout: Duration,
    /// Fallback ids for v2 permission subjects without a tool-call id.
    request_sequence: Arc<AtomicU64>,
}

/// The requests a worker is holding open, shared with the socket loop.
///
/// Shared rather than owned by the broker because the answer arrives on a
/// different task: the broker is inside a provider's ACP client, while the
/// resolution arrives from the worker's socket. Both must see the same table.
///
/// Keyed by the worker's own `request_id`, which is unique within a run, and
/// tagged with the run so one run ending settles only *its* open requests — a
/// worker can be running several threads at once, and cancelling another thread's
/// question would block it for no reason.
#[derive(Clone, Default)]
pub struct PermissionRegistry {
    pending: Arc<Mutex<HashMap<String, PendingPermission>>>,
}

/// One held request: who is waiting, and for which run.
struct PendingPermission {
    run_id: loom_domain::RunId,
    answer: oneshot::Sender<InteractionAnswer>,
}

impl PermissionRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a waiter, returning `false` when the id is already held.
    async fn insert(
        &self,
        request_id: String,
        run_id: loom_domain::RunId,
        tx: oneshot::Sender<InteractionAnswer>,
    ) -> bool {
        let mut pending = self.pending.lock().await;
        if pending.contains_key(&request_id) {
            return false;
        }
        pending.insert(request_id, PendingPermission { run_id, answer: tx });
        true
    }

    async fn remove(&self, request_id: &str) {
        self.pending.lock().await.remove(request_id);
    }

    /// Hands an answer to the request waiting for it, if any.
    ///
    /// `false` means no request is waiting — the common case for a redelivered
    /// frame, or a resolution for a run that already ended. The caller treats it
    /// as a no-op rather than an error, which is what makes redelivery safe.
    pub async fn resolve(&self, request_id: &str, answer: InteractionAnswer) -> bool {
        let Some(pending) = self.pending.lock().await.remove(request_id) else {
            return false;
        };
        pending.answer.send(answer).is_ok()
    }

    /// Hands a waiter the control plane's refusal, if one is waiting.
    ///
    /// A request the control plane will not record has no client that can ever
    /// answer it: the question never became an entity, so nothing will publish a
    /// resolution for it. The agent is told "not granted" now instead of at the
    /// end of the permission timeout, which is what the operator sees as a turn
    /// that hangs on a question their UI never showed.
    ///
    /// `false` means nothing was waiting, which is a no-op like [`Self::resolve`]
    /// — a redelivered acknowledgement, or a run that ended while it was in
    /// flight.
    pub async fn refuse(&self, request_id: &str, reason: &str) -> bool {
        self.resolve(
            request_id,
            InteractionAnswer::Cancelled {
                reason: reason.to_owned(),
            },
        )
        .await
    }

    /// Asks one run's held requests to settle as cancelled, returning how many.
    ///
    /// Called when that run ends: a turn that finished cannot still be blocked on
    /// a question, and a request whose answer can no longer arrive must not keep
    /// the agent waiting. Scoped by run because a worker serves several threads
    /// concurrently — settling *every* request here would cancel a question
    /// another run is legitimately waiting on.
    pub async fn cancel_run(&self, run_id: &loom_domain::RunId, reason: &str) -> usize {
        let mut pending = self.pending.lock().await;
        let doomed: Vec<String> = pending
            .iter()
            .filter(|(_, entry)| &entry.run_id == run_id)
            .map(|(id, _)| id.clone())
            .collect();
        let mut cancelled = 0;
        for id in doomed {
            if let Some(entry) = pending.remove(&id) {
                if entry
                    .answer
                    .send(InteractionAnswer::Cancelled {
                        reason: reason.to_owned(),
                    })
                    .is_ok()
                {
                    cancelled += 1;
                }
            }
        }
        cancelled
    }

    /// Asks every held request to settle as cancelled, returning how many.
    ///
    /// Called when the connection drops or the worker is stopping: at that point
    /// nothing can answer *any* request, so settling all of them is the correct
    /// reading rather than an over-broad one.
    pub async fn cancel_all(&self, reason: &str) -> usize {
        let mut pending = self.pending.lock().await;
        let mut cancelled = 0;
        for (_, entry) in pending.drain() {
            if entry
                .answer
                .send(InteractionAnswer::Cancelled {
                    reason: reason.to_owned(),
                })
                .is_ok()
            {
                cancelled += 1;
            }
        }
        cancelled
    }

    /// How many requests are held open.
    pub async fn len(&self) -> usize {
        self.pending.lock().await.len()
    }

    /// Whether nothing is held open.
    pub async fn is_empty(&self) -> bool {
        self.len().await == 0
    }
}

fn permission_option_v1(option: v1::PermissionOption) -> NormalizedPermissionOption {
    NormalizedPermissionOption {
        id: option.option_id.0.to_string(),
        name: option.name,
        kind: match option.kind {
            v1::PermissionOptionKind::AllowOnce => PermissionChoiceKind::AllowOnce,
            v1::PermissionOptionKind::AllowAlways => PermissionChoiceKind::AllowAlways,
            v1::PermissionOptionKind::RejectOnce => PermissionChoiceKind::RejectOnce,
            v1::PermissionOptionKind::RejectAlways => PermissionChoiceKind::RejectAlways,
            _ => PermissionChoiceKind::Other,
        },
    }
}

fn permission_option_v2(option: v2::PermissionOption) -> NormalizedPermissionOption {
    NormalizedPermissionOption {
        id: option.option_id.0.to_string(),
        name: option.name,
        kind: match option.kind {
            v2::PermissionOptionKind::AllowOnce => PermissionChoiceKind::AllowOnce,
            v2::PermissionOptionKind::AllowAlways => PermissionChoiceKind::AllowAlways,
            v2::PermissionOptionKind::RejectOnce => PermissionChoiceKind::RejectOnce,
            v2::PermissionOptionKind::RejectAlways => PermissionChoiceKind::RejectAlways,
            _ => PermissionChoiceKind::Other,
        },
    }
}

impl PermissionPrompt {
    /// Parses the v1 request without routing it through the v2 schema.
    fn from_v1(request: v1::RequestPermissionRequest) -> Self {
        let session_id = request.session_id.0.to_string();
        let tool_call = request.tool_call;
        let item_id = tool_call.tool_call_id.0.to_string();
        let title = tool_call
            .fields
            .title
            .clone()
            .unwrap_or_else(|| "permission requested".to_owned());
        let subject = PermissionSubject::ToolCall {
            item_id: item_id.clone(),
            tool: tool_call
                .fields
                .kind
                .map(tool_kind_name_v1)
                .unwrap_or("other")
                .to_owned(),
            title: tool_call.fields.title.clone(),
            raw_input: tool_call.fields.raw_input.clone(),
            raw_output: tool_call.fields.raw_output.clone(),
        };
        Self {
            request_id: format!("{session_id}:{item_id}"),
            session_id,
            title,
            description: None,
            subject,
            options: request
                .options
                .into_iter()
                .map(permission_option_v1)
                .collect(),
        }
    }

    /// Parses the v2 request while retaining v2-only subjects and description.
    fn from_v2(request: v2::RequestPermissionRequest, request_id: String) -> Self {
        let session_id = request.session_id.0.to_string();
        let title = request.title.clone();
        let description = request.description.clone();
        let subject = match request.subject {
            Some(v2::RequestPermissionSubject::ToolCall(subject)) => {
                let tool_call = subject.tool_call;
                PermissionSubject::ToolCall {
                    item_id: tool_call.tool_call_id.0.to_string(),
                    tool: tool_call
                        .kind
                        .value()
                        .map(tool_kind_name_v2)
                        .unwrap_or("other")
                        .to_owned(),
                    title: tool_call.title.value().cloned(),
                    raw_input: tool_call.raw_input.value().cloned(),
                    raw_output: tool_call.raw_output.value().cloned(),
                }
            }
            Some(v2::RequestPermissionSubject::Command(subject)) => PermissionSubject::Command {
                item_id: subject
                    .tool_call_id
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_else(|| request_id.clone()),
                command: subject.command,
                cwd: subject.cwd.0.to_string_lossy().into_owned(),
                terminal_id: subject.terminal_id.map(|id| id.to_string()),
            },
            Some(v2::RequestPermissionSubject::Other(subject)) => PermissionSubject::Other {
                type_name: subject.type_.clone(),
                raw: serde_json::to_value(subject).unwrap_or(Value::Null),
            },
            Some(subject) => PermissionSubject::Other {
                type_name: "unknown".to_owned(),
                raw: serde_json::to_value(subject).unwrap_or(Value::Null),
            },
            None => PermissionSubject::None,
        };
        Self {
            session_id,
            request_id,
            title,
            description,
            subject,
            options: request
                .options
                .into_iter()
                .map(permission_option_v2)
                .collect(),
        }
    }
}

impl PermissionBroker {
    /// Builds a broker for one run.
    pub fn new(
        run: ProviderRun,
        outbound: mpsc::Sender<InteractionRequest>,
        pending: PermissionRegistry,
        timeout: Duration,
    ) -> Self {
        Self {
            run,
            outbound,
            pending,
            timeout,
            request_sequence: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Handles a native ACP v1 permission request.
    pub async fn ask_v1(
        &self,
        request: v1::RequestPermissionRequest,
    ) -> v1::RequestPermissionResponse {
        let prompt = PermissionPrompt::from_v1(request);
        let answer = self.ask_prompt(prompt.clone()).await;
        permission_response_v1(answer, &prompt, self.run.permission_ceiling)
    }

    /// Handles a native ACP v2 permission request.
    pub async fn ask_v2(
        &self,
        request: v2::RequestPermissionRequest,
    ) -> v2::RequestPermissionResponse {
        let request_id = self.request_id_v2(&request);
        let prompt = PermissionPrompt::from_v2(request, request_id);
        let answer = self.ask_prompt(prompt.clone()).await;
        permission_response_v2(answer, &prompt, self.run.permission_ceiling)
    }

    /// Test compatibility for the existing v1-only broker tests. Production
    /// callbacks use the explicitly versioned methods above.
    #[cfg(test)]
    async fn ask(&self, request: v1::RequestPermissionRequest) -> v1::RequestPermissionResponse {
        self.ask_v1(request).await
    }

    /// Holds one version-neutral prompt open until the control plane answers it.
    async fn ask_prompt(&self, prompt: PermissionPrompt) -> InteractionAnswer {
        let (tx, rx) = oneshot::channel();
        if !self
            .pending
            .insert(prompt.request_id.clone(), self.run.run_id.clone(), tx)
            .await
        {
            return InteractionAnswer::Cancelled {
                reason: "duplicate permission request id".to_owned(),
            };
        }
        let _guard = PendingGuard {
            registry: self.pending.clone(),
            request_id: prompt.request_id.clone(),
        };

        let frame = InteractionRequest {
            host_id: self.run.host_id.clone(),
            run_id: self.run.run_id.clone(),
            thread_id: self.run.thread_id.clone(),
            project_id: self.run.project_id.clone(),
            request_id: prompt.request_id.clone(),
            provider_thread_id: Some(prompt.session_id.clone()),
            kind: InteractionKind::Approval,
            payload: InteractionPayload::new(InteractionKind::Approval, approval_payload(&prompt)),
            expires_at_ms: None,
        };
        if self.outbound.send(frame).await.is_err() {
            return InteractionAnswer::Cancelled {
                reason: "the worker connection closed".to_owned(),
            };
        }

        match tokio::time::timeout(self.timeout, rx).await {
            Ok(Ok(answer)) => answer,
            Ok(Err(_)) => InteractionAnswer::Cancelled {
                reason: "the permission waiter was dropped".to_owned(),
            },
            Err(_) => InteractionAnswer::Cancelled {
                reason: "the permission request timed out".to_owned(),
            },
        }
    }

    /// v2 command and extension subjects do not always have a tool-call id.
    fn request_id_v2(&self, request: &v2::RequestPermissionRequest) -> String {
        let session_id = request.session_id.0.to_string();
        let subject_id = match request.subject.as_ref() {
            Some(v2::RequestPermissionSubject::ToolCall(subject)) => {
                Some(subject.tool_call.tool_call_id.0.to_string())
            }
            Some(v2::RequestPermissionSubject::Command(subject)) => subject
                .tool_call_id
                .as_ref()
                .map(ToString::to_string)
                .or_else(|| subject.terminal_id.as_ref().map(ToString::to_string)),
            Some(v2::RequestPermissionSubject::Other(_)) | None => None,
            Some(_) => None,
        };
        subject_id
            .map(|subject_id| format!("{session_id}:{subject_id}"))
            .unwrap_or_else(|| {
                let sequence = self.request_sequence.fetch_add(1, Ordering::Relaxed);
                format!("{session_id}:permission-{sequence}")
            })
    }
}

/// Removes a pending entry when the asking future goes away.
struct PendingGuard {
    registry: PermissionRegistry,
    request_id: String,
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        let registry = self.registry.clone();
        let request_id = std::mem::take(&mut self.request_id);
        // `Drop` cannot await. The removal is a lock acquisition on an
        // uncontended map, so it is done on a task; a request that outlives the
        // dropped future is answered by `cancel_run` when its run ends, or by
        // `cancel_all` on disconnect, at the latest — and `resolve` on a stale
        // id is a documented no-op.
        tokio::spawn(async move {
            registry.remove(&request_id).await;
        });
    }
}

/// Builds the contract approval payload from the version-neutral prompt.
fn approval_payload(prompt: &PermissionPrompt) -> Value {
    let decisions = {
        let mut decisions = Vec::new();
        if prompt
            .options
            .iter()
            .any(|option| option.kind == PermissionChoiceKind::AllowOnce)
        {
            decisions.push("allow_once");
        }
        if prompt
            .options
            .iter()
            .any(|option| option.kind == PermissionChoiceKind::AllowAlways)
        {
            decisions.push("allow_for_session");
        }
        // A refusal is always representable — a rejecting option if the agent
        // offered one, `Cancelled` otherwise — so `deny` is always offered.
        decisions.push("deny");
        decisions
    };

    let subject = match &prompt.subject {
        PermissionSubject::ToolCall {
            item_id,
            tool,
            title,
            ..
        } => {
            let label = title.as_deref().unwrap_or(&prompt.title);
            json!({
                "kind": "tool_use",
                "itemId": item_id,
                "tool": tool,
                "presentation": {
                    "label": { "pending": label, "completed": label },
                    "icon": { "glyph": "Lock" },
                },
            })
        }
        PermissionSubject::Command {
            item_id,
            command,
            cwd,
            ..
        } => json!({
            "kind": "command",
            "itemId": item_id,
            "command": command,
            "cwd": cwd,
            "actions": [{ "type": "unknown", "command": command }],
            "sessionGrant": null,
        }),
        // The contract has no open-ended approval subject. Keep the request
        // generic rather than pretending an unknown v2 subject was a command;
        // the raw subject remains preserved in PermissionPrompt until this
        // product projection is replaced by a richer contract variant.
        PermissionSubject::Other { .. } | PermissionSubject::None => json!({
            "kind": "tool_use",
            "itemId": prompt.request_id,
            "tool": "other",
            "presentation": {
                "label": { "pending": prompt.title, "completed": prompt.title },
                "icon": { "glyph": "Lock" },
            },
        }),
    };

    let reason = prompt
        .description
        .clone()
        .or_else(|| match &prompt.subject {
            PermissionSubject::ToolCall {
                raw_input, title, ..
            } => raw_input
                .as_ref()
                .and_then(|input| input.get("title"))
                .and_then(Value::as_str)
                .map(str::to_owned)
                .or_else(|| title.clone()),
            _ => Some(prompt.title.clone()),
        });
    json!({
        "kind": "approval",
        "subject": subject,
        "reason": reason,
        "availableDecisions": decisions,
    })
}

/// A tool kind as the adapter's stable lower-case name.
fn tool_kind_name_v1(kind: v1::ToolKind) -> &'static str {
    use v1::ToolKind;
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

fn tool_kind_name_v2(kind: &v2::ToolKind) -> &'static str {
    use v2::ToolKind;
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
        ToolKind::Other | ToolKind::Unknown(_) => "other",
        _ => "other",
    }
}

/// Selects the agent's option id for a control-plane decision.
fn selected_option_id(
    answer: InteractionAnswer,
    prompt: &PermissionPrompt,
    ceiling: HostPermissionMode,
) -> Option<String> {
    let InteractionAnswer::Decision { decision } = answer else {
        return None;
    };
    let decision =
        if decision == PermissionDecision::AllowForSession && ceiling != HostPermissionMode::Full {
            PermissionDecision::AllowOnce
        } else {
            decision
        };
    let wanted: &[PermissionChoiceKind] = match decision {
        PermissionDecision::AllowOnce => &[PermissionChoiceKind::AllowOnce],
        PermissionDecision::AllowForSession => &[
            PermissionChoiceKind::AllowAlways,
            PermissionChoiceKind::AllowOnce,
        ],
        PermissionDecision::Deny => &[
            PermissionChoiceKind::RejectOnce,
            PermissionChoiceKind::RejectAlways,
        ],
    };
    wanted.iter().find_map(|kind| {
        prompt
            .options
            .iter()
            .find(|option| option.kind == *kind)
            .map(|option| option.id.clone())
    })
}

fn permission_response_v1(
    answer: InteractionAnswer,
    prompt: &PermissionPrompt,
    ceiling: HostPermissionMode,
) -> v1::RequestPermissionResponse {
    match selected_option_id(answer, prompt, ceiling) {
        Some(option_id) => v1::RequestPermissionResponse::new(
            v1::RequestPermissionOutcome::Selected(v1::SelectedPermissionOutcome::new(option_id)),
        ),
        None => v1::RequestPermissionResponse::new(v1::RequestPermissionOutcome::Cancelled),
    }
}

fn permission_response_v2(
    answer: InteractionAnswer,
    prompt: &PermissionPrompt,
    ceiling: HostPermissionMode,
) -> v2::RequestPermissionResponse {
    match selected_option_id(answer, prompt, ceiling) {
        Some(option_id) => v2::RequestPermissionResponse::new(
            v2::RequestPermissionOutcome::Selected(v2::SelectedPermissionOutcome::new(option_id)),
        ),
        None => v2::RequestPermissionResponse::new(v2::RequestPermissionOutcome::Cancelled),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol_schema::v1::{
        PermissionOption, PermissionOptionKind, RequestPermissionOutcome, RequestPermissionRequest,
        SelectedPermissionOutcome, SessionId, ToolCallId, ToolCallUpdate, ToolCallUpdateFields,
    };
    use loom_domain::{HostId, ProjectId, RunId, ThreadId};
    use loom_provider_protocol::ProviderSpec;

    fn run() -> ProviderRun {
        ProviderRun {
            spec: ProviderSpec::pi(),
            prompt: "go".into(),
            host_id: HostId::mint(),
            thread_id: ThreadId::mint(),
            project_id: ProjectId::mint(),
            run_id: RunId::mint(),
            timeout: Duration::from_secs(30),
            ceiling: crate::DEFAULT_RUN_CEILING,
            permission_timeout: Duration::from_secs(30),
            settle_timeout: crate::DEFAULT_SETTLE_TIMEOUT,
            permission_ceiling: HostPermissionMode::Full,
            provider_session_id: None,
            model: None,
            reasoning_level: None,
        }
    }

    fn request(options: Vec<PermissionOption>) -> RequestPermissionRequest {
        let fields = ToolCallUpdateFields::new()
            .kind(Some(agent_client_protocol_schema::v1::ToolKind::Execute))
            .title(Some("Run the test suite".into()));
        RequestPermissionRequest::new(
            SessionId::new("sess-1"),
            ToolCallUpdate::new(ToolCallId::new("call-1"), fields),
            options,
        )
    }

    fn allow_options() -> Vec<PermissionOption> {
        vec![
            PermissionOption::new("allow-once", "Allow once", PermissionOptionKind::AllowOnce),
            PermissionOption::new("deny", "Deny", PermissionOptionKind::RejectOnce),
        ]
    }

    fn broker(timeout: Duration) -> (PermissionBroker, mpsc::Receiver<InteractionRequest>) {
        let (tx, rx) = mpsc::channel(8);
        let broker = PermissionBroker::new(run(), tx, PermissionRegistry::new(), timeout);
        (broker, rx)
    }

    #[tokio::test]
    async fn a_v2_command_permission_stays_native_and_keeps_its_description() {
        let (broker, mut outbound) = broker(Duration::from_secs(5));
        let pending = broker.pending.clone();
        let request = v2::RequestPermissionRequest::new(
            "sess-v2",
            "Run the command",
            vec![
                v2::PermissionOption::new(
                    "allow-once",
                    "Allow once",
                    v2::PermissionOptionKind::AllowOnce,
                ),
                v2::PermissionOption::new("deny", "Deny", v2::PermissionOptionKind::RejectOnce),
            ],
        )
        .description("The agent needs to run a build")
        .subject(v2::RequestPermissionSubject::from(
            v2::CommandPermissionSubject::new("cargo test", "/workspace").tool_call_id("call-v2"),
        ));
        let handle = tokio::spawn(async move { broker.ask_v2(request).await });

        let frame = outbound.recv().await.expect("the v2 question travels up");
        assert_eq!(frame.request_id, "sess-v2:call-v2");
        assert_eq!(
            frame.payload.body["reason"],
            "The agent needs to run a build"
        );
        assert_eq!(frame.payload.body["subject"]["kind"], "command");
        assert_eq!(frame.payload.body["subject"]["command"], "cargo test");
        assert_eq!(frame.payload.body["subject"]["cwd"], "/workspace");
        assert_eq!(
            frame.payload.body["subject"]["actions"][0]["type"],
            "unknown"
        );

        assert!(
            pending
                .resolve(
                    &frame.request_id,
                    InteractionAnswer::Decision {
                        decision: PermissionDecision::AllowOnce,
                    },
                )
                .await
        );
        let response = handle.await.unwrap();
        assert_eq!(
            response.outcome,
            v2::RequestPermissionOutcome::Selected(v2::SelectedPermissionOutcome::new(
                "allow-once"
            ))
        );
    }

    /// ever answer it. The agent must be told that immediately: the timeout is
    /// for a human who has not got round to clicking, not for a question that
    /// never reached anyone.
    #[tokio::test]
    async fn a_request_the_control_plane_refuses_is_cancelled_at_once() {
        // Generous enough that the assertion below cannot pass by waiting it
        // out: five minutes is the deployed default, and the test would time
        // out long before.
        let (broker, mut outbound) = broker(Duration::from_secs(300));
        let pending = broker.pending.clone();
        let handle = tokio::spawn(async move { broker.ask(request(allow_options())).await });

        let frame = outbound.recv().await.expect("the request travels up");
        assert!(
            pending
                .refuse(&frame.request_id, "run run_1 is not in flight")
                .await,
            "the waiter is handed the refusal"
        );

        let response = tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("a refused request is answered now, not at the permission timeout")
            .unwrap();
        assert_eq!(
            response.outcome,
            RequestPermissionOutcome::Cancelled,
            "a refusal is never an approval"
        );

        // A refusal for something this worker is not holding — a redelivered
        // acknowledgement, or a run that ended first — is a no-op the socket
        // loop logs rather than an error.
        assert!(
            !pending.refuse(&frame.request_id, "late").await,
            "the waiter was consumed by the refusal"
        );
    }

    #[tokio::test]
    async fn a_request_travels_up_and_the_answer_comes_back() {
        let (broker, mut outbound) = broker(Duration::from_secs(5));
        let pending = broker.pending.clone();
        let handle = tokio::spawn(async move { broker.ask(request(allow_options())).await });

        // The frame the socket loop would forward.
        let frame = outbound.recv().await.expect("the request travels up");
        assert_eq!(frame.request_id, "sess-1:call-1");
        assert_eq!(frame.kind, InteractionKind::Approval);
        assert_eq!(
            frame.provider_thread_id.as_deref(),
            Some("sess-1"),
            "the agent's session id is what a client corrrelates the question with"
        );
        assert_eq!(frame.payload.body["kind"], "approval");
        assert_eq!(
            frame.payload.body["availableDecisions"],
            serde_json::json!(["allow_once", "deny"])
        );
        assert_eq!(
            frame.payload.body["subject"]["kind"], "tool_use",
            "the tool call is the subject"
        );

        // The answer arrives on the worker's socket, not as a reply to the
        // frame, so it is routed through the shared registry.
        assert!(
            pending
                .resolve(
                    &frame.request_id,
                    InteractionAnswer::Decision {
                        decision: PermissionDecision::AllowOnce
                    }
                )
                .await
        );

        let response = handle.await.unwrap();
        assert_eq!(
            response.outcome,
            RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new("allow-once"))
        );
    }

    #[tokio::test]
    async fn an_allow_once_never_picks_the_session_wide_option() {
        let (broker, mut outbound) = broker(Duration::from_secs(5));
        let pending = broker.pending.clone();
        let options = vec![
            PermissionOption::new(
                "allow-always",
                "Always allow",
                PermissionOptionKind::AllowAlways,
            ),
            PermissionOption::new("allow-once", "Allow once", PermissionOptionKind::AllowOnce),
        ];
        let handle = tokio::spawn(async move { broker.ask(request(options)).await });
        let frame = outbound.recv().await.unwrap();
        pending
            .resolve(
                &frame.request_id,
                InteractionAnswer::Decision {
                    decision: PermissionDecision::AllowOnce,
                },
            )
            .await;

        // The agent's own ordering puts the session-wide option first; a
        // once-decision must not reach for it.
        assert_eq!(
            handle.await.unwrap().outcome,
            RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new("allow-once"))
        );
    }

    #[tokio::test]
    async fn an_allow_for_session_prefers_the_session_wide_option() {
        let (broker, mut outbound) = broker(Duration::from_secs(5));
        let pending = broker.pending.clone();
        let options = vec![
            PermissionOption::new("allow-once", "Allow once", PermissionOptionKind::AllowOnce),
            PermissionOption::new(
                "allow-session",
                "Allow for session",
                PermissionOptionKind::AllowAlways,
            ),
        ];
        let handle = tokio::spawn(async move { broker.ask(request(options)).await });
        let frame = outbound.recv().await.unwrap();
        pending
            .resolve(
                &frame.request_id,
                InteractionAnswer::Decision {
                    decision: PermissionDecision::AllowForSession,
                },
            )
            .await;

        assert_eq!(
            handle.await.unwrap().outcome,
            RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new("allow-session"))
        );
    }

    /// A permission request whose options cannot be told apart by polarity
    /// resolves to the agent's own first option of the wanted kind.
    ///
    /// This is a **documented lossiness**, kept because the alternative is worse.
    /// loom's contract types an approval's answer as one of three decisions, and
    /// its response schema rejects an opaque resolution on an approval — so there
    /// is no conformant way to name an ACP `optionId`. The worker therefore maps
    /// the decision onto the agent's ordering, which is the only rule that does
    /// not invent a choice the user did not make. A permission request whose
    /// options *are* distinguishable by polarity (the normal ACP shape: an
    /// allowing option and a rejecting one) is unaffected.
    #[tokio::test]
    async fn a_multi_choice_request_resolves_by_the_agents_own_ordering() {
        let (broker, mut outbound) = broker(Duration::from_secs(5));
        let pending = broker.pending.clone();
        // Two options that both allow: a polarity cannot tell them apart.
        let options = vec![
            PermissionOption::new("alpha", "Pick alpha", PermissionOptionKind::AllowOnce),
            PermissionOption::new("beta", "Pick beta", PermissionOptionKind::AllowOnce),
        ];
        let handle = tokio::spawn(async move { broker.ask(request(options)).await });
        let frame = outbound.recv().await.unwrap();
        pending
            .resolve(
                &frame.request_id,
                InteractionAnswer::Decision {
                    decision: PermissionDecision::AllowOnce,
                },
            )
            .await;

        assert_eq!(
            handle.await.unwrap().outcome,
            RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new("alpha")),
            "the agent's own first allowing option is chosen; loom does not invent one"
        );
    }

    #[tokio::test]
    async fn a_deny_cancels_when_the_agent_offered_no_rejecting_option() {
        let (broker, mut outbound) = broker(Duration::from_secs(5));
        let pending = broker.pending.clone();
        // Only allowing options: there is no rejecting one to select, and
        // answering `Selected` with an allow would silently grant what the user
        // refused.
        let options = vec![PermissionOption::new(
            "allow-once",
            "Allow once",
            PermissionOptionKind::AllowOnce,
        )];
        let handle = tokio::spawn(async move { broker.ask(request(options)).await });
        let frame = outbound.recv().await.unwrap();
        assert_eq!(
            frame.payload.body["availableDecisions"],
            serde_json::json!(["allow_once", "deny"]),
            "a refusal must still be representable"
        );
        pending
            .resolve(
                &frame.request_id,
                InteractionAnswer::Decision {
                    decision: PermissionDecision::Deny,
                },
            )
            .await;
        assert_eq!(
            handle.await.unwrap().outcome,
            RequestPermissionOutcome::Cancelled
        );
    }

    #[tokio::test]
    async fn nothing_answered_is_cancelled_never_granted() {
        // A client that never answers must not produce an allow. This is the
        // regression the old auto-allow policy was.
        let (broker, mut outbound) = broker(Duration::from_millis(20));
        let handle = tokio::spawn(async move { broker.ask(request(allow_options())).await });
        let _ = outbound.recv().await.expect("the question was asked");
        let response = handle.await.unwrap();
        assert_eq!(response.outcome, RequestPermissionOutcome::Cancelled);
    }

    #[tokio::test]
    async fn a_dropped_connection_settles_every_open_request() {
        let (broker, mut outbound) = broker(Duration::from_secs(30));
        let pending = broker.pending.clone();
        let handle = tokio::spawn({
            let broker = broker.clone();
            async move { broker.ask(request(allow_options())).await }
        });
        let _ = outbound.recv().await.unwrap();
        assert_eq!(pending.len().await, 1);

        assert_eq!(pending.cancel_all("the socket closed").await, 1);
        assert_eq!(
            handle.await.unwrap().outcome,
            RequestPermissionOutcome::Cancelled
        );
        assert!(pending.is_empty().await);
    }

    /// One run ending settles only *its* requests.
    ///
    /// A worker serves several threads concurrently. Cancelling every held
    /// request when one turn finished would block another thread's agent on a
    /// question that is still perfectly answerable.
    #[tokio::test]
    async fn ending_one_run_does_not_cancel_another_runs_question() {
        let pending = PermissionRegistry::new();
        let first = run();
        let mut second = run();
        // A different run id, so the two are distinguishable.
        second.run_id = RunId::mint();

        let (first_tx, first_rx) = oneshot::channel();
        let (second_tx, second_rx) = oneshot::channel();
        assert!(
            pending
                .insert("sess:a".into(), first.run_id.clone(), first_tx)
                .await
        );
        assert!(
            pending
                .insert("sess:b".into(), second.run_id.clone(), second_tx)
                .await
        );

        assert_eq!(pending.cancel_run(&first.run_id, "run one ended").await, 1);
        // The first run's waiter is settled; the second's is untouched.
        assert_eq!(
            first_rx.await.unwrap(),
            InteractionAnswer::Cancelled {
                reason: "run one ended".into()
            }
        );
        assert_eq!(pending.len().await, 1);
        assert!(
            pending
                .resolve(
                    "sess:b",
                    InteractionAnswer::Decision {
                        decision: PermissionDecision::AllowOnce
                    }
                )
                .await
        );
        assert!(matches!(
            second_rx.await.unwrap(),
            InteractionAnswer::Decision { .. }
        ));
    }

    #[tokio::test]
    async fn a_resolution_for_an_unknown_request_is_a_no_op() {
        // Redelivery: the relay may hand the worker the same frame twice, and a
        // frame for a run that already ended must not be an error.
        let registry = PermissionRegistry::new();
        assert!(
            !registry
                .resolve(
                    "no-such-request",
                    InteractionAnswer::Cancelled {
                        reason: "late".into()
                    }
                )
                .await
        );
    }

    #[tokio::test]
    async fn a_closed_socket_cancels_rather_than_blocking() {
        let (tx, rx) = mpsc::channel(1);
        drop(rx);
        let broker = PermissionBroker::new(
            run(),
            tx,
            PermissionRegistry::new(),
            Duration::from_secs(30),
        );
        assert_eq!(
            broker.ask(request(allow_options())).await.outcome,
            RequestPermissionOutcome::Cancelled
        );
    }
}
