//! Interaction lifecycle: recording a provider's question and delivering the
//! answer.
//!
//! This mirrors [`crate::runs`] in shape, and the parallel is the point: a
//! provider's request for input is *observed* by the worker on the same stream
//! as everything else, and the control plane turns the observation into a
//! durable entity exactly as it turns a terminal frame into a thread status
//! change.
//!
//! # Why the control plane owns this at all
//!
//! A question is not a runtime concern, because the answer has to survive a
//! disconnect: the worker may report a question, lose its socket, and only
//! receive the answer after a restart. So the interaction lives in the entity
//! view ([`crate::domain_state`]) and its changes travel through the relay's
//! thread scope like every other fact. The worker never holds a question open
//! on a socket.
//!
//! # Delivery protocol
//!
//! The provider protocol carries an [`InteractionResolutionFrame`] back to the
//! worker. Delivery is two-phase: accepting an answer moves the durable row to
//! `resolving`, the host-scoped frame is appended to the replayable relay, and
//! only then does the row become `resolved` (or `interrupted` for cancellation).
//! A relay failure therefore leaves a durable delivery intent instead of a
//! hidden, settled row with a provider still blocked. The run reconciler retries
//! every resolving interaction until delivery succeeds or run teardown cancels
//! it.

use loom_domain::{
    Interaction, InteractionId, InteractionKind, InteractionOrigin, InteractionPayload,
    InteractionStatus, NewInteraction, Resolution, RunId, ThreadId,
};
use loom_provider_protocol::{
    InteractionAnswer, InteractionRequest, InteractionResolutionFrame, PermissionDecision,
};
use loom_relay::{Result as RelayResult, Scope};

use crate::domain_state::CommandError;
use crate::state::AppState;

/// What happened when a worker reported a permission request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecordOutcome {
    /// The question was recorded (or was already known) and is now visible to
    /// clients.
    Recorded(Box<Interaction>),
    /// The request named a run that is not in flight, or one this host does not
    /// own. Nothing was recorded: a question with no turn cannot be answered in
    /// any client's timeline.
    Unknown(String),
}

/// What happened when an answer was delivered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeliverOutcome {
    /// The provider answer is replayable and the interaction is terminal.
    Delivered(Box<Interaction>),
    /// The answer is retained in `resolving` for the reconciler to retry.
    DeliveryFailed {
        interaction: Box<Interaction>,
        error: String,
    },
    /// The interaction does not exist.
    Unknown,
    /// The interaction was already settled by someone else.
    Settled(Box<Interaction>),
}

impl AppState {
    /// Records a provider's interaction request.
    ///
    /// `provider_request_id` is the provider's own identity for the request and
    /// is what makes a redelivery idempotent: the interaction id is derived
    /// from it, so the second report of the same question lands on the same
    /// row. A provider that reports no request id gets a fresh row, because
    /// there is nothing to deduplicate against.
    #[allow(clippy::too_many_arguments)]
    pub fn record_interaction(
        &self,
        thread_id: &ThreadId,
        turn_id: &str,
        kind: InteractionKind,
        origin: InteractionOrigin,
        payload: InteractionPayload,
        provider_request_id: Option<&str>,
        provider_thread_id: Option<&str>,
        expires_at_ms: Option<u64>,
        now_ms: u64,
    ) -> Result<Interaction, CommandError> {
        let id = provider_request_id.map(deterministic_interaction_id);
        let (interaction, event) = self.registry.create_interaction(
            NewInteraction {
                thread_id: thread_id.clone(),
                turn_id: turn_id.to_owned(),
                kind,
                origin,
                payload,
                expires_at_ms,
                provider_thread_id: provider_thread_id.map(str::to_owned),
                id,
            },
            now_ms,
        )?;
        if let Some(event) = event {
            let _ = self.publish_domain_event(&event);
        }
        Ok(interaction)
    }

    /// Accepts an answer, persists its delivery intent, and finishes only after
    /// the worker frame is replayable.
    pub fn deliver_interaction_resolution(
        &self,
        interaction_id: &InteractionId,
        resolution: Resolution,
        now_ms: u64,
    ) -> DeliverOutcome {
        let Some(existing) = self.registry.interaction(interaction_id) else {
            return DeliverOutcome::Unknown;
        };
        let resolution = self.normalize_permission_resolution(&existing, resolution);
        match self
            .registry
            .prepare_interaction_resolution(interaction_id, resolution, now_ms)
        {
            Ok((interaction, event)) => {
                if let Err(error) = self.publish_domain_event(&event) {
                    return Self::delivery_failed(interaction, error);
                }
                self.finish_prepared_resolution(interaction, now_ms)
            }
            Err(CommandError::NotFound(_)) => DeliverOutcome::Unknown,
            Err(_) => self
                .registry
                .interaction(interaction_id)
                .map_or(DeliverOutcome::Unknown, |current| {
                    DeliverOutcome::Settled(Box::new(current))
                }),
        }
    }

    fn normalize_permission_resolution(
        &self,
        interaction: &Interaction,
        resolution: Resolution,
    ) -> Resolution {
        match resolution {
            Resolution::Decision {
                decision,
                granted_permissions,
            } if decision == "allow_for_session"
                && interaction
                    .turn_id
                    .parse::<RunId>()
                    .ok()
                    .and_then(|run_id| self.runs.get(&run_id))
                    .and_then(|run| self.registry.host(&run.host_id))
                    .is_some_and(|host| {
                        host.max_permission_mode != loom_domain::HostPermissionMode::Full
                    }) =>
            {
                Resolution::Decision {
                    decision: "allow_once".into(),
                    granted_permissions,
                }
            }
            other => other,
        }
    }

    fn finish_prepared_resolution(&self, interaction: Interaction, now_ms: u64) -> DeliverOutcome {
        let Some(resolution) = interaction.resolution.as_ref() else {
            return DeliverOutcome::Settled(Box::new(interaction));
        };
        if let Err(error) = self.publish_interaction_resolution(&interaction, resolution, now_ms) {
            return Self::delivery_failed(interaction, error);
        }
        match self
            .registry
            .complete_interaction_resolution(&interaction.id, now_ms)
        {
            Ok((completed, event)) => {
                if let Err(error) = self.publish_domain_event(&event) {
                    eprintln!(
                        "loom-server: provider received interaction {} but its terminal event failed: {error}",
                        completed.id
                    );
                }
                DeliverOutcome::Delivered(Box::new(completed))
            }
            Err(CommandError::NotFound(_)) => DeliverOutcome::Unknown,
            Err(_) => self
                .registry
                .interaction(&interaction.id)
                .map_or(DeliverOutcome::Unknown, |current| {
                    DeliverOutcome::Settled(Box::new(current))
                }),
        }
    }

    /// Cancels an interaction and tells a blocked provider the request ended.
    pub fn cancel_interaction(
        &self,
        interaction_id: &InteractionId,
        reason: Option<String>,
        now_ms: u64,
    ) -> DeliverOutcome {
        match self
            .registry
            .prepare_interaction_cancellation(interaction_id, reason, now_ms)
        {
            Ok((interaction, event)) => {
                if let Err(error) = self.publish_domain_event(&event) {
                    return Self::delivery_failed(interaction, error);
                }
                self.finish_prepared_cancellation(interaction, now_ms)
            }
            Err(CommandError::NotFound(_)) => DeliverOutcome::Unknown,
            Err(_) => self
                .registry
                .interaction(interaction_id)
                .map_or(DeliverOutcome::Unknown, |current| {
                    DeliverOutcome::Settled(Box::new(current))
                }),
        }
    }

    fn finish_prepared_cancellation(
        &self,
        interaction: Interaction,
        now_ms: u64,
    ) -> DeliverOutcome {
        let reason = interaction
            .status_reason
            .clone()
            .unwrap_or_else(|| "interaction cancelled".to_owned());
        if let Err(error) = self.publish_interaction_answer(
            &interaction,
            InteractionAnswer::Cancelled { reason },
            now_ms,
        ) {
            return Self::delivery_failed(interaction, error);
        }
        match self
            .registry
            .complete_interaction_cancellation(&interaction.id, now_ms)
        {
            Ok((completed, event)) => {
                if let Err(error) = self.publish_domain_event(&event) {
                    eprintln!(
                        "loom-server: provider received interaction cancellation {} but its terminal event failed: {error}",
                        completed.id
                    );
                }
                DeliverOutcome::Delivered(Box::new(completed))
            }
            Err(CommandError::NotFound(_)) => DeliverOutcome::Unknown,
            Err(_) => self
                .registry
                .interaction(&interaction.id)
                .map_or(DeliverOutcome::Unknown, |current| {
                    DeliverOutcome::Settled(Box::new(current))
                }),
        }
    }

    /// Retries every provider answer retained in the `resolving` state.
    pub(crate) fn retry_resolving_interactions(&self, now_ms: u64) -> usize {
        let mut delivered = 0;
        for interaction in self
            .registry
            .interactions()
            .into_iter()
            .filter(|interaction| interaction.status == InteractionStatus::Resolving)
        {
            let event = loom_domain::DomainEvent::ThreadInteractionChanged {
                interaction: interaction.clone(),
            };
            if self.publish_domain_event(&event).is_err() {
                continue;
            }
            let outcome = if interaction.resolution.is_some() {
                self.finish_prepared_resolution(interaction, now_ms)
            } else {
                self.finish_prepared_cancellation(interaction, now_ms)
            };
            if matches!(outcome, DeliverOutcome::Delivered(_)) {
                delivered += 1;
            }
        }
        delivered
    }

    fn delivery_failed(interaction: Interaction, error: impl std::fmt::Display) -> DeliverOutcome {
        DeliverOutcome::DeliveryFailed {
            interaction: Box::new(interaction),
            error: error.to_string(),
        }
    }

    /// Cancels every open interaction a thread has, returning how many.
    ///
    /// Called when a run ends: a terminal turn cannot still be waiting on an
    /// answer, so the interactions it raised are settled with it. This is the
    /// invariant that keeps `hasPendingInteraction` honest in the thread list.
    pub fn cancel_thread_interactions(&self, thread_id: &ThreadId, now_ms: u64) -> usize {
        let open = self
            .registry
            .interactions_for(Some(thread_id))
            .into_iter()
            .filter(|interaction| !interaction.status.is_terminal())
            .collect::<Vec<_>>();
        let mut cancelled = 0;
        for interaction in open {
            if self
                .registry
                .cancel_interaction(
                    &interaction.id,
                    Some("the turn ended before the interaction was answered".into()),
                    now_ms,
                )
                .is_ok()
            {
                cancelled += 1;
            }
        }
        cancelled
    }

    /// Records a provider's permission request and publishes it to clients.
    ///
    /// This is the ACP `session/request_permission` producer the interaction
    /// routes were missing, and it enforces the same ownership rules a run
    /// report does: the run must be in flight and owned by the requesting host.
    /// A question for a run nobody is advancing is refused rather than stored,
    /// because there is no client that could ever render or answer it.
    ///
    /// Idempotent on `request_id`: the interaction id is derived from it, so a
    /// redelivered frame lands on the same row. An already-known request returns
    /// the existing interaction unchanged — including when it is already
    /// settled, which is exactly what a worker replaying its held requests
    /// after a reconnect needs.
    pub fn record_interaction_request(
        &self,
        request: InteractionRequest,
        now_ms: u64,
    ) -> RecordOutcome {
        let Some(record) = self.runs.get(&request.run_id) else {
            return RecordOutcome::Unknown(format!("run {} is not in flight", request.run_id));
        };
        if record.host_id != request.host_id {
            return RecordOutcome::Unknown(format!(
                "run {} is owned by host {}, not {}",
                request.run_id, record.host_id, request.host_id
            ));
        }
        if record.thread_id != request.thread_id {
            return RecordOutcome::Unknown(format!(
                "run {} belongs to thread {}, not {}",
                request.run_id, record.thread_id, request.thread_id
            ));
        }
        // The contract requires a turn id and loom's turn *is* the run, so the
        // run id is the honest value rather than a placeholder.
        let turn_id = request.run_id.to_string();
        let origin = InteractionOrigin::Provider {
            provider_id: self.provider_spec().name.clone(),
            provider_request_id: request.request_id.clone(),
        };
        // The dedup key is scoped by run: ACP's request ids are unique only
        // within a session, and a worker that restarts would otherwise be able
        // to collide a fresh question with a settled row from an earlier run.
        // The provider's own id stays verbatim in `origin`, so the answer frame
        // can name it.
        let scoped_request_id = format!("{}/{}", request.run_id, request.request_id);
        match self.record_interaction(
            &request.thread_id,
            &turn_id,
            request.kind,
            origin,
            request.payload,
            Some(&scoped_request_id),
            request.provider_thread_id.as_deref(),
            request.expires_at_ms,
            now_ms,
        ) {
            Ok(interaction) => {
                let interaction = self
                    .registry
                    .interaction(&interaction.id)
                    .unwrap_or(interaction);
                // The agent's session id is what a client correlates the
                // question with the run events' `providerThreadId`. It arrives
                // with the request rather than being looked up, so recording it
                // needs no second read.
                RecordOutcome::Recorded(Box::new(interaction))
            }
            Err(error) => RecordOutcome::Unknown(error.to_string()),
        }
    }

    /// Publishes a resolved permission request to the answering host's scope.
    ///
    /// Only a typed decision can resolve an approval. Client cancellation uses
    /// the same transport through [`Self::publish_interaction_answer`], but is
    /// kept separate here so an unknown resolution can never become an allow.
    fn publish_interaction_resolution(
        &self,
        interaction: &Interaction,
        resolution: &Resolution,
        now_ms: u64,
    ) -> RelayResult<()> {
        let answer = match resolution {
            Resolution::Decision { decision, .. } => {
                let decision = match decision.as_str() {
                    "allow_once" => PermissionDecision::AllowOnce,
                    "allow_for_session" => PermissionDecision::AllowForSession,
                    // `deny` is the contract's only remaining decision, and an
                    // unknown word cannot be turned into an allow.
                    _ => PermissionDecision::Deny,
                };
                InteractionAnswer::Decision { decision }
            }
            // The other resolutions cannot answer an approval; the route
            // already refused them, so reaching here means the interaction
            // changed. Publishing nothing is strictly better than guessing.
            _ => return Ok(()),
        };
        self.publish_interaction_answer(interaction, answer, now_ms)
    }

    /// Sends one provider approval answer through the replayable host scope.
    fn publish_interaction_answer(
        &self,
        interaction: &Interaction,
        answer: InteractionAnswer,
        now_ms: u64,
    ) -> RelayResult<()> {
        if interaction.kind != InteractionKind::Approval {
            return Ok(());
        }
        let InteractionOrigin::Provider {
            provider_request_id,
            ..
        } = &interaction.origin
        else {
            return Ok(());
        };
        // The run that asked. `turn_id` is the run id (see
        // `record_interaction_request`); a request that reached the domain by
        // another path may carry a different turn id, in which case there is no
        // run to address the frame to and nothing is published.
        let Ok(run_id) = interaction.turn_id.parse() else {
            return Ok(());
        };
        let Some(record) = self.runs.get(&run_id) else {
            // Interactions can also be created by non-provider API/plugin
            // paths. With no in-flight run there is no worker waiter to unblock.
            return Ok(());
        };
        let frame = InteractionResolutionFrame {
            host_id: record.host_id.clone(),
            run_id: record.run_id.clone(),
            thread_id: record.thread_id.clone(),
            request_id: provider_request_id.clone(),
            interaction_id: interaction.id.to_string(),
            answer,
            created_at_ms: now_ms,
        };
        let payload = serde_json::to_vec(&frame).expect("an interaction frame always serializes");
        self.publish(Scope::Host(record.host_id.to_string()), payload)
            .map(|_| ())
    }
}

/// A provider request id as an interaction id.
///
/// The id has to be a valid `intr_…` value and stable for one provider request,
/// so the provider's own string cannot be used verbatim. A hash over it gives
/// both: the same request id always yields the same interaction, so a
/// redelivered question is one row rather than two. FNV-1a is deliberate — the
/// relay already uses it for sharding, so the codebase has one hash function and
/// not two.
fn deterministic_interaction_id(provider_request_id: &str) -> InteractionId {
    /// The Crockford base32 alphabet the id body uses.
    const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in provider_request_id.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    // 26 base32 characters carry 130 bits, and a 64-bit hash padded into that
    // value leaves the top two bits clear, which is exactly the ULID shape
    // `InteractionId::parse` enforces. The id is not time-ordered — it names a
    // provider request, not a creation — but it is stable and unique per hash.
    let mut bytes = [0u8; 26];
    let mut value = u128::from(hash);
    for slot in bytes.iter_mut().rev() {
        *slot = ALPHABET[(value & 0x1F) as usize];
        value >>= 5;
    }
    let body = String::from_utf8(bytes.to_vec()).expect("the base32 alphabet is ASCII");
    InteractionId::parse(&format!("intr_{body}")).unwrap_or_else(|_| InteractionId::mint())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_provider_request_id_maps_to_a_stable_interaction_id() {
        let first = deterministic_interaction_id("req-1");
        let again = deterministic_interaction_id("req-1");
        let other = deterministic_interaction_id("req-2");
        assert_eq!(first, again, "the same request must map to the same row");
        assert_ne!(first, other);
        assert!(first.to_string().starts_with("intr_"));
    }

    // --- the permission bridge -------------------------------------------

    use crate::state::AppConfig;
    use loom_domain::{EnvironmentKind, MessageRole, RunEvent, Thread};
    use loom_relay::backend::memory::MemoryBackend;
    use loom_relay::backend::{LogRecord, RelayBackend};
    use loom_relay::{now_ms, EventId, RelayError, ShardId};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    struct FailOnAppendBackend {
        inner: MemoryBackend,
        append_count: AtomicUsize,
        fail_at: AtomicUsize,
    }

    impl FailOnAppendBackend {
        fn new() -> Self {
            Self {
                inner: MemoryBackend::new(2_000),
                append_count: AtomicUsize::new(0),
                fail_at: AtomicUsize::new(0),
            }
        }

        fn fail_on_append_after_next(&self) {
            let after_next = self.append_count.load(Ordering::Acquire).saturating_add(2);
            self.fail_at.store(after_next, Ordering::Release);
        }

        fn disable_failure(&self) {
            self.fail_at.store(0, Ordering::Release);
        }
    }

    impl RelayBackend for FailOnAppendBackend {
        fn shard_count(&self) -> u8 {
            self.inner.shard_count()
        }

        fn append(&self, shard: ShardId, record: LogRecord) -> loom_relay::Result<()> {
            let append_number = self.append_count.fetch_add(1, Ordering::AcqRel) + 1;
            if self.fail_at.load(Ordering::Acquire) == append_number {
                return Err(RelayError::backend(format!(
                    "injected append failure at append {append_number}"
                )));
            }
            self.inner.append(shard, record)
        }

        fn read_after(
            &self,
            shard: ShardId,
            after: Option<EventId>,
            limit: usize,
        ) -> loom_relay::Result<Vec<LogRecord>> {
            self.inner.read_after(shard, after, limit)
        }

        fn trim(&self, shard: ShardId, before_ms: u64) -> loom_relay::Result<u64> {
            self.inner.trim(shard, before_ms)
        }

        fn len(&self, shard: ShardId) -> loom_relay::Result<usize> {
            self.inner.len(shard)
        }
    }

    /// A state that neither reconciles nor snapshots, so a test drives it.
    fn state() -> AppState {
        AppState::build(AppConfig {
            reconcile_interval: std::time::Duration::ZERO,
            ..AppConfig::default()
        })
        .unwrap()
    }

    fn state_with_backend(backend: Arc<FailOnAppendBackend>) -> AppState {
        AppState::build_for_test(
            AppConfig {
                reconcile_interval: std::time::Duration::ZERO,
                ..AppConfig::default()
            },
            backend,
        )
        .unwrap()
    }

    /// A connected host with a thread bound to a workspace, and one run
    /// dispatched for it. Returns the host, the thread, and the run.
    fn dispatched_run(
        state: &AppState,
        workspace: &str,
    ) -> (loom_domain::HostId, Thread, loom_domain::RunId) {
        let (host, _) = state
            .registry
            .enroll_host(None, "laptop".into(), now_ms())
            .unwrap();
        let (environment, _) = state
            .registry
            .create_environment(
                Some(state.registry.personal_project_id()),
                host.id.clone(),
                EnvironmentKind::Unmanaged,
                Some(workspace.into()),
                now_ms(),
            )
            .unwrap();
        let (thread, _) = state
            .registry
            .create_thread(
                Some(state.registry.personal_project_id()),
                Some("t".into()),
                Some(environment.id),
                now_ms(),
            )
            .unwrap();
        state
            .registry
            .post_message(&thread.id, MessageRole::User, "go".into(), now_ms())
            .unwrap();
        let thread = state.registry.thread(&thread.id).unwrap();
        let run = match state.dispatch_thread(&thread, "go") {
            crate::runs::DispatchOutcome::Dispatched(run) => run,
            other => panic!("expected a dispatch, got {other:?}"),
        };
        (host.id, thread, run.run_id)
    }

    /// A permission request for a run.
    fn request(
        host_id: &loom_domain::HostId,
        thread: &Thread,
        run_id: &loom_domain::RunId,
        request_id: &str,
    ) -> InteractionRequest {
        InteractionRequest {
            host_id: host_id.clone(),
            run_id: run_id.clone(),
            thread_id: thread.id.clone(),
            project_id: thread.project_id.clone(),
            request_id: request_id.into(),
            provider_thread_id: Some("acp-session-1".into()),
            kind: InteractionKind::Approval,
            payload: InteractionPayload::new(
                InteractionKind::Approval,
                serde_json::json!({
                    "kind": "approval",
                    "subject": {
                        "kind": "tool_use",
                        "itemId": "call-1",
                        "tool": "execute",
                        "presentation": {
                            "label": { "pending": "Run it", "completed": "Ran it" },
                            "icon": { "glyph": "Terminal" },
                        },
                    },
                    "reason": null,
                    "availableDecisions": ["allow_once", "deny"],
                }),
            ),
            expires_at_ms: None,
        }
    }

    /// Every host-scope payload, parsed.
    fn host_frames(state: &AppState, host_id: &loom_domain::HostId) -> Vec<serde_json::Value> {
        state
            .relay
            .replay_scope(&Scope::Host(host_id.to_string()), 50)
            .unwrap()
            .into_iter()
            .filter_map(|envelope| {
                let frame: serde_json::Value = serde_json::from_slice(&envelope.payload).ok()?;
                serde_json::from_str(frame["payload"].as_str()?).ok()
            })
            .collect()
    }

    #[tokio::test]
    async fn a_permission_request_becomes_a_durable_interaction_and_its_answer_a_frame() {
        let state = state();
        let (host_id, thread, run_id) = dispatched_run(&state, "/srv/project-a");

        // The request is recorded against the run's turn, carries the agent's
        // session id, and is open.
        let recorded = state
            .record_interaction_request(request(&host_id, &thread, &run_id, "call-1"), now_ms());
        let outcome = match recorded {
            RecordOutcome::Recorded(interaction) => interaction,
            other => panic!("expected a recorded request, got {other:?}"),
        };
        assert_eq!(outcome.status, loom_domain::InteractionStatus::Pending);
        assert_eq!(outcome.turn_id, run_id.to_string());
        assert_eq!(outcome.provider_thread_id.as_deref(), Some("acp-session-1"));
        assert_eq!(state.registry.pending_interactions(&thread.id).len(), 1);

        // A redelivery lands on the same row rather than asking twice.
        let again = state
            .record_interaction_request(request(&host_id, &thread, &run_id, "call-1"), now_ms());
        let again = match again {
            RecordOutcome::Recorded(interaction) => interaction,
            other => panic!("expected the same request, got {other:?}"),
        };
        assert_eq!(again.id, outcome.id, "one provider request is one row");
        assert_eq!(state.registry.pending_interactions(&thread.id).len(), 1);

        // The answer travels to the host scope, naming the worker's own request
        // id so it can match the request it is holding open.
        let delivered = state.deliver_interaction_resolution(
            &outcome.id,
            Resolution::Decision {
                decision: "deny".into(),
                granted_permissions: None,
            },
            now_ms(),
        );
        assert!(matches!(delivered, DeliverOutcome::Delivered(_)));
        let frames = host_frames(&state, &host_id);
        let frame = frames
            .iter()
            .find(|frame| frame["request_id"] == "call-1")
            .expect("the resolution reached the host scope");
        assert_eq!(frame["interaction_id"], outcome.id.to_string());
        assert_eq!(frame["run_id"], run_id.to_string());
        assert_eq!(frame["thread_id"], thread.id.to_string());
        assert_eq!(frame["answer"]["kind"], "decision");
        assert_eq!(frame["answer"]["decision"], "deny");

        state.shutdown();
    }

    #[tokio::test]
    async fn a_host_permission_ceiling_downgrades_session_scope_before_delivery() {
        let state = state();
        let (host_id, thread, run_id) = dispatched_run(&state, "/srv/project-a");
        state
            .registry
            .update_host_permission_ceiling(
                &host_id,
                loom_domain::HostPermissionMode::Auto,
                now_ms(),
            )
            .unwrap();
        let interaction = match state.record_interaction_request(
            request(&host_id, &thread, &run_id, "call-ceiling"),
            now_ms(),
        ) {
            RecordOutcome::Recorded(interaction) => interaction,
            other => panic!("expected a recorded request, got {other:?}"),
        };

        let outcome = state.deliver_interaction_resolution(
            &interaction.id,
            Resolution::Decision {
                decision: "allow_for_session".into(),
                granted_permissions: Some(serde_json::Value::Null),
            },
            now_ms(),
        );
        let DeliverOutcome::Delivered(interaction) = outcome else {
            panic!("expected a delivered answer, got {outcome:?}");
        };
        assert_eq!(
            interaction.resolution,
            Some(Resolution::Decision {
                decision: "allow_once".into(),
                granted_permissions: Some(serde_json::Value::Null),
            })
        );
        let frame = host_frames(&state, &host_id)
            .into_iter()
            .find(|frame| frame["request_id"] == "call-ceiling")
            .expect("the downgraded answer reached the host");
        assert_eq!(frame["answer"]["decision"], "allow_once");

        state.shutdown();
    }

    #[tokio::test]
    async fn a_failed_provider_append_stays_resolving_until_reconciliation_retries_it() {
        let backend = Arc::new(FailOnAppendBackend::new());
        let state = state_with_backend(backend.clone());
        let (host_id, thread, run_id) = dispatched_run(&state, "/srv/project-a");
        let interaction = match state
            .record_interaction_request(request(&host_id, &thread, &run_id, "call-retry"), now_ms())
        {
            RecordOutcome::Recorded(interaction) => interaction,
            other => panic!("expected a recorded request, got {other:?}"),
        };

        // The resolving domain event is the next append. Fail the host-scoped
        // answer immediately after it, leaving the durable intent retryable.
        backend.fail_on_append_after_next();
        let outcome = state.deliver_interaction_resolution(
            &interaction.id,
            Resolution::Decision {
                decision: "deny".into(),
                granted_permissions: None,
            },
            now_ms(),
        );
        assert!(matches!(outcome, DeliverOutcome::DeliveryFailed { .. }));
        assert_eq!(
            state.registry.interaction(&interaction.id).unwrap().status,
            InteractionStatus::Resolving
        );
        assert!(
            host_frames(&state, &host_id)
                .iter()
                .all(|frame| frame["request_id"] != "call-retry"),
            "a failed append must not look delivered"
        );

        backend.disable_failure();
        assert_eq!(state.retry_resolving_interactions(now_ms()), 1);
        assert_eq!(
            state.registry.interaction(&interaction.id).unwrap().status,
            InteractionStatus::Resolved
        );
        let frame = host_frames(&state, &host_id)
            .into_iter()
            .find(|frame| frame["request_id"] == "call-retry")
            .expect("reconciliation must make the answer replayable");
        assert_eq!(frame["answer"]["decision"], "deny");

        state.shutdown();
    }

    #[tokio::test]
    async fn a_client_cancellation_becomes_a_provider_cancellation_frame() {
        let state = state();
        let (host_id, thread, run_id) = dispatched_run(&state, "/srv/project-a");
        let interaction = match state.record_interaction_request(
            request(&host_id, &thread, &run_id, "call-cancel"),
            now_ms(),
        ) {
            RecordOutcome::Recorded(interaction) => interaction,
            other => panic!("expected a recorded request, got {other:?}"),
        };

        let cancelled = match state.cancel_interaction(
            &interaction.id,
            Some("cancelled by a client".into()),
            now_ms(),
        ) {
            DeliverOutcome::Delivered(interaction) => *interaction,
            other => panic!("expected delivered cancellation, got {other:?}"),
        };
        assert_eq!(
            cancelled.status,
            loom_domain::InteractionStatus::Interrupted
        );
        assert!(cancelled.resolution.is_none());

        let frames = host_frames(&state, &host_id);
        let frame = frames
            .iter()
            .find(|frame| frame["request_id"] == "call-cancel")
            .expect("the cancellation reached the host scope");
        assert_eq!(frame["interaction_id"], interaction.id.to_string());
        assert_eq!(frame["answer"]["kind"], "cancelled");
        assert_eq!(frame["answer"]["reason"], "cancelled by a client");

        state.shutdown();
    }

    #[tokio::test]
    async fn a_failed_cancellation_append_is_retried_without_becoming_a_denial() {
        let backend = Arc::new(FailOnAppendBackend::new());
        let state = state_with_backend(backend.clone());
        let (host_id, thread, run_id) = dispatched_run(&state, "/srv/project-a");
        let interaction = match state.record_interaction_request(
            request(&host_id, &thread, &run_id, "cancel-retry"),
            now_ms(),
        ) {
            RecordOutcome::Recorded(interaction) => interaction,
            other => panic!("expected a recorded request, got {other:?}"),
        };

        backend.fail_on_append_after_next();
        let outcome = state.cancel_interaction(
            &interaction.id,
            Some("cancelled by a client".into()),
            now_ms(),
        );
        assert!(matches!(outcome, DeliverOutcome::DeliveryFailed { .. }));
        let pending = state.registry.interaction(&interaction.id).unwrap();
        assert_eq!(pending.status, InteractionStatus::Resolving);
        assert!(pending.resolution.is_none());

        backend.disable_failure();
        assert_eq!(state.retry_resolving_interactions(now_ms()), 1);
        assert_eq!(
            state.registry.interaction(&interaction.id).unwrap().status,
            InteractionStatus::Interrupted
        );
        let frame = host_frames(&state, &host_id)
            .into_iter()
            .find(|frame| frame["request_id"] == "cancel-retry")
            .expect("reconciliation must make the cancellation replayable");
        assert_eq!(frame["answer"]["kind"], "cancelled");
        assert!(frame["answer"].get("decision").is_none());

        state.shutdown();
    }

    #[tokio::test]
    async fn a_request_for_a_run_this_host_does_not_own_is_refused() {
        let state = state();
        let (host_id, thread, run_id) = dispatched_run(&state, "/srv/project-a");
        // Host scoping is what stops one machine answering for another's run.
        let stranger = loom_domain::HostId::mint();
        assert!(matches!(
            state.record_interaction_request(
                request(&stranger, &thread, &run_id, "call-1"),
                now_ms()
            ),
            RecordOutcome::Unknown(_)
        ));
        assert!(state.registry.pending_interactions(&thread.id).is_empty());
        // And the real host still works, so the refusal was not a false negative.
        assert!(matches!(
            state.record_interaction_request(
                request(&host_id, &thread, &run_id, "call-1"),
                now_ms()
            ),
            RecordOutcome::Recorded(_)
        ));
        state.shutdown();
    }

    #[tokio::test]
    async fn a_request_for_an_unknown_run_is_refused() {
        let state = state();
        let (host_id, thread, _) = dispatched_run(&state, "/srv/project-a");
        // A run id nobody dispatched: there is no turn to render the question
        // in, so storing it would create an unanswerable row.
        let unknown = loom_domain::RunId::mint();
        assert!(matches!(
            state.record_interaction_request(
                request(&host_id, &thread, &unknown, "call-1"),
                now_ms()
            ),
            RecordOutcome::Unknown(_)
        ));
        assert!(state.registry.pending_interactions(&thread.id).is_empty());
        state.shutdown();
    }

    #[tokio::test]
    async fn ending_the_run_cancels_its_open_request_and_stops_the_frame() {
        let state = state();
        let (host_id, thread, run_id) = dispatched_run(&state, "/srv/project-a");
        let outcome = match state
            .record_interaction_request(request(&host_id, &thread, &run_id, "call-1"), now_ms())
        {
            RecordOutcome::Recorded(interaction) => interaction,
            other => panic!("expected a recorded request, got {other:?}"),
        };
        let before = host_frames(&state, &host_id).len();

        // A terminal report ends the turn, and the open question settles with
        // it rather than staying `pending` forever.
        let terminal = RunEvent::completed(
            thread.id.clone(),
            thread.project_id.clone(),
            run_id.clone(),
            now_ms(),
            None,
        );
        assert_eq!(
            state.apply_run_report(
                &host_id,
                loom_provider_protocol::ProviderReport {
                    host_id: host_id.clone(),
                    event: terminal,
                }
            ),
            crate::runs::ReportOutcome::Applied
        );
        assert_eq!(
            state.registry.interaction(&outcome.id).unwrap().status,
            loom_domain::InteractionStatus::Interrupted,
        );

        // A later answer cannot unblock anything: the run is gone, so no frame
        // is published for it.
        let _ = state.deliver_interaction_resolution(
            &outcome.id,
            Resolution::Decision {
                decision: "allow_once".into(),
                granted_permissions: Some(serde_json::json!({
                    "network": { "enabled": true },
                    "fileSystem": { "read": [], "write": [] },
                })),
            },
            now_ms(),
        );
        assert_eq!(
            host_frames(&state, &host_id).len(),
            before,
            "an answer for an ended run must not reach the worker"
        );
        state.shutdown();
    }

    #[test]
    fn a_derived_id_is_a_valid_interaction_id() {
        // Whatever the provider sends, the derived id must parse — it is
        // deserialized on the way back out of the log and the snapshot.
        for request in [
            "",
            "a",
            "provider/request:42",
            "\u{1F600}",
            &"x".repeat(500),
        ] {
            let id = deterministic_interaction_id(request);
            assert_eq!(InteractionId::parse(&id.to_string()).unwrap(), id);
        }
    }
}
