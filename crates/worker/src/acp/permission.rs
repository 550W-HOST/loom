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
use std::sync::Arc;
use std::time::Duration;

use agent_client_protocol::schema::v1::{
    PermissionOptionKind, RequestPermissionOutcome, RequestPermissionRequest,
    RequestPermissionResponse, SelectedPermissionOutcome,
};
use loom_domain::{HostPermissionMode, InteractionKind, InteractionPayload};
use loom_provider_protocol::{InteractionAnswer, InteractionRequest, PermissionDecision};
use tokio::sync::{mpsc, oneshot, Mutex};

use crate::provider::ProviderRun;

/// How long a permission request waits for a user by default.
///
/// Long enough that a human notices the dialog and answers, short enough that a
/// run blocked with no client attached does not hold its thread in `working`
/// until the run deadline. The run deadline remains the outer bound; this is the
/// tighter one for the question itself.
pub const DEFAULT_PERMISSION_TIMEOUT: Duration = Duration::from_secs(5 * 60);

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
        }
    }

    /// Answers one ACP permission request with the *user's* decision.
    ///
    /// Blocks the ACP connection's dispatch loop until the answer arrives, the
    /// control plane refuses the request, or [`PermissionBroker::timeout`]
    /// passes. That is deliberate: the agent is itself blocked on this request,
    /// so no further ACP traffic is expected while it is open, and holding the
    /// loop preserves the ordering between the question and the updates around
    /// it.
    pub async fn ask(&self, request: RequestPermissionRequest) -> RequestPermissionResponse {
        let request_id = self.request_id(&request);
        let (tx, rx) = oneshot::channel();
        if !self
            .pending
            .insert(request_id.clone(), self.run.run_id.clone(), tx)
            .await
        {
            // A duplicate request id inside one run: ACP's tool call ids are
            // unique, so this means a retried request the broker is already
            // holding. Cancelling is the honest answer rather than answering
            // either copy with the other's decision.
            return cancelled();
        }
        // The insertion is undone when this future is dropped — a run timeout,
        // for instance — so a stale waiter cannot be answered later.
        let _guard = PendingGuard {
            registry: self.pending.clone(),
            request_id: request_id.clone(),
        };

        let frame = InteractionRequest {
            host_id: self.run.host_id.clone(),
            run_id: self.run.run_id.clone(),
            thread_id: self.run.thread_id.clone(),
            project_id: self.run.project_id.clone(),
            request_id: request_id.clone(),
            // The ACP session is the agent's own identity for the conversation,
            // which is exactly what a client correlates the question with the
            // `providerThreadId` on the run events around it.
            provider_thread_id: Some(request.session_id.0.to_string()),
            kind: InteractionKind::Approval,
            payload: InteractionPayload::new(InteractionKind::Approval, approval_payload(&request)),
            expires_at_ms: None,
        };
        if self.outbound.send(frame).await.is_err() {
            // The socket loop is gone, so nothing can answer. Cancel rather
            // than block, and never assume approval.
            return cancelled();
        }

        match tokio::time::timeout(self.timeout, rx).await {
            Ok(Ok(answer)) => answer_to_response(answer, &request, self.run.permission_ceiling),
            // The waiter was dropped — the refusal path is a hand-off like any
            // answer, so a refused request arrives at the arm above with
            // `InteractionAnswer::Cancelled`. Either way there is no answer to
            // wait for.
            Ok(Err(_)) => cancelled(),
            // Nobody answered in time. The agent must be unblocked, and the
            // only truthful answer is "not granted".
            Err(_) => cancelled(),
        }
    }

    /// The worker's identity for one ACP request.
    ///
    /// ACP identifies a tool call by `toolCallId`, which is unique within a
    /// session, and a session is one run, so the pair `(session, tool call)` is
    /// unique within the run the control plane scopes the dedup key by. The
    /// session id is included so a request is traceable in a log without a
    /// lookup.
    fn request_id(&self, request: &RequestPermissionRequest) -> String {
        format!(
            "{}:{}",
            request.session_id.0, request.tool_call.tool_call_id.0
        )
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

/// The contract's approval payload for an ACP permission request.
///
/// The subject is the tool call the agent is asking about, as
/// [`loom_domain::InteractionKind::Approval`]'s `tool_use` branch requires. The
/// available decisions are the contract's three words, which every ACP option
/// set can be answered with: an allowing option satisfies an `allow`, a
/// rejecting one a `deny`, and an option set with neither leaves only `deny`.
fn approval_payload(request: &RequestPermissionRequest) -> serde_json::Value {
    let title = request
        .tool_call
        .fields
        .title
        .clone()
        .unwrap_or_else(|| "permission requested".to_owned());
    let mut decisions = Vec::new();
    if request
        .options
        .iter()
        .any(|option| matches!(option.kind, PermissionOptionKind::AllowOnce))
    {
        decisions.push("allow_once");
    }
    if request
        .options
        .iter()
        .any(|option| matches!(option.kind, PermissionOptionKind::AllowAlways))
    {
        decisions.push("allow_for_session");
    }
    // A refusal is always representable — a rejecting option if the agent
    // offered one, `Cancelled` otherwise — so `deny` is always offered. The
    // opposite is not true: a request whose options all reject has no `allow`.
    decisions.push("deny");
    serde_json::json!({
        "kind": "approval",
        "subject": {
            "kind": "tool_use",
            "itemId": request.tool_call.tool_call_id.0.to_string(),
            "tool": request
                .tool_call
                .fields
                .kind
                .map(tool_kind_name)
                .unwrap_or("other"),
            "presentation": {
                "label": { "pending": title, "completed": title },
                "icon": { "glyph": "Lock" },
            },
        },
        // The contract requires `reason`; the agent's own title is the closest
        // true thing to it, and the tool call's title is already what a client
        // displays. No reason is fabricated when the agent gave none.
        "reason": request
            .tool_call
            .fields
            .raw_input
            .as_ref()
            .and_then(|input| input.get("title"))
            .and_then(serde_json::Value::as_str)
            .or(request.tool_call.fields.title.as_deref()),
        "availableDecisions": decisions,
    })
}

/// A tool kind as the adapter's stable lower-case name.
fn tool_kind_name(kind: agent_client_protocol_schema::v1::ToolKind) -> &'static str {
    use agent_client_protocol_schema::v1::ToolKind;
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

/// The ACP response for a decision the client made.
///
/// An `allow` picks the agent's own option with matching scope. A once-decision
/// never widens to session scope; a session decision prefers `AllowAlways` and
/// falls back to `AllowOnce` only when the agent did not expose a durable
/// option. The host permission ceiling may explicitly downgrade session scope
/// before this mapping. A `deny` picks a rejecting option; when the agent
/// offered none, `Cancelled` is the only truthful reply, because "there was a
/// rejecting option and the user picked it" and "the request was withdrawn"
/// are not the same fact and ACP has no third word.
fn answer_to_response(
    answer: InteractionAnswer,
    request: &RequestPermissionRequest,
    ceiling: HostPermissionMode,
) -> RequestPermissionResponse {
    match answer {
        InteractionAnswer::Decision { decision } => {
            let decision = if decision == PermissionDecision::AllowForSession
                && ceiling != HostPermissionMode::Full
            {
                PermissionDecision::AllowOnce
            } else {
                decision
            };
            let wanted: &[PermissionOptionKind] = match decision {
                PermissionDecision::AllowOnce => &[PermissionOptionKind::AllowOnce],
                PermissionDecision::AllowForSession => &[
                    PermissionOptionKind::AllowAlways,
                    PermissionOptionKind::AllowOnce,
                ],
                PermissionDecision::Deny => &[
                    PermissionOptionKind::RejectOnce,
                    PermissionOptionKind::RejectAlways,
                ],
            };
            match pick(request, wanted) {
                Some(option_id) => selected(option_id),
                None => cancelled(),
            }
        }
        // The option id is the agent's own. Trusting it verbatim is the point:
        // re-deriving it from the option's kind would pick the wrong option
        // when several options allow the same thing, which is exactly what a
        // multi-choice permission request looks like.
        InteractionAnswer::Cancelled { .. } => cancelled(),
    }
}

/// The first option whose kind is in `wanted`, by the agent's own ordering.
fn pick(request: &RequestPermissionRequest, wanted: &[PermissionOptionKind]) -> Option<String> {
    for kind in wanted {
        if let Some(option) = request.options.iter().find(|option| option.kind == *kind) {
            return Some(option.option_id.0.to_string());
        }
    }
    None
}

fn selected(option_id: String) -> RequestPermissionResponse {
    RequestPermissionResponse::new(RequestPermissionOutcome::Selected(
        SelectedPermissionOutcome::new(option_id),
    ))
}

fn cancelled() -> RequestPermissionResponse {
    RequestPermissionResponse::new(RequestPermissionOutcome::Cancelled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol_schema::v1::{
        PermissionOption, SessionId, ToolCallId, ToolCallUpdate, ToolCallUpdateFields,
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
            permission_timeout: Duration::from_secs(30),
            permission_ceiling: HostPermissionMode::Full,
            provider_session_id: None,
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

    /// The control plane can refuse to record a question, and then nothing will
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
