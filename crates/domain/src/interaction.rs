//! Interactions: a provider asking the user for something mid-turn.
//!
//! An *interaction* is a request the agent makes to a human — "may I run this
//! command?", "which of these two do you want?", "the plugin needs input" — and
//! the record of what the human answered. It is durable for the same reason a
//! queued message is: the run that raised it may be reported by a worker after
//! a server restart, and a client must be able to see and answer a question it
//! did not witness arrive.
//!
//! # The state machine
//!
//! ```text
//!   pending ──resolve/respond──▶ resolving ──delivery stored──▶ resolved
//!      │                            │
//!      └────────cancel──────────────┴──delivery stored────────▶ interrupted
//! ```
//!
//! Three verbs, three different things, and the difference is the reason
//! `threads.interaction` is not spelled as one route:
//!
//! * **respond** carries an opaque `value` — the answer for an interaction
//!   whose payload loom does not interpret, stored verbatim.
//! * **resolve** carries a *typed* resolution — a permission decision, a set of
//!   question answers, a plugin submission — and refuses a resolution whose
//!   kind does not match the interaction's payload.
//! * **cancel** settles the interaction as `interrupted` without an answer:
//!   the run was stopped, or the provider went away, and there is nothing to
//!   answer any more.
//!
//! `resolving` exists because an answer is a two-step fact: the server has
//! accepted it and retained the delivery intent, but the replayable worker
//! frame is not stored yet. A client that sees `resolving` keeps the row
//! disabled; `resolved` means the provider answer can be delivered or replayed.
//! Cancellation uses the same intermediate state so a relay failure cannot hide
//! a prompt while its provider remains blocked.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::error::DomainError;
use crate::id::{InteractionId, ProjectId, ThreadId};

/// Where an interaction is in its life.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InteractionStatus {
    /// Waiting for the user.
    #[default]
    Pending,
    /// An answer was accepted and is being delivered.
    Resolving,
    /// The provider has the answer.
    Resolved,
    /// Settled without an answer: the run ended first.
    Interrupted,
}

impl InteractionStatus {
    /// Whether the interaction still accepts an answer.
    ///
    /// `resolving` does not: a second answer while the first is in flight is
    /// exactly the double-answer the status exists to prevent.
    pub fn is_open(self) -> bool {
        matches!(self, InteractionStatus::Pending)
    }

    /// Whether nothing can change it any more.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            InteractionStatus::Resolved | InteractionStatus::Interrupted
        )
    }

    /// The stable wire token.
    pub fn as_str(self) -> &'static str {
        match self {
            InteractionStatus::Pending => "pending",
            InteractionStatus::Resolving => "resolving",
            InteractionStatus::Resolved => "resolved",
            InteractionStatus::Interrupted => "interrupted",
        }
    }
}

impl fmt::Display for InteractionStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What kind of request an interaction carries.
///
/// The set is closed to the contract's four payload shapes. A provider frame
/// that fits none of them becomes [`InteractionKind::Generic`] with its body
/// kept verbatim, rather than being dropped: an unmodelled question still has
/// to reach the human who can answer it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InteractionKind {
    /// A permission decision with a set of allowed answers.
    Approval,
    /// A provider's multiple-choice/free-text question.
    UserQuestion,
    /// An opaque request whose body loom does not interpret.
    #[default]
    Generic,
    /// A plugin's own request.
    Plugin,
}

impl InteractionKind {
    /// The `payload.kind` this interaction is answered with.
    pub fn payload_kind(self) -> &'static str {
        match self {
            InteractionKind::Approval => "approval",
            InteractionKind::UserQuestion => "user_question",
            InteractionKind::Generic => "generic",
            InteractionKind::Plugin => "plugin",
        }
    }

    /// The stable wire token.
    pub fn as_str(self) -> &'static str {
        match self {
            InteractionKind::Approval => "approval",
            InteractionKind::UserQuestion => "user_question",
            InteractionKind::Generic => "generic",
            InteractionKind::Plugin => "plugin",
        }
    }
}

impl fmt::Display for InteractionKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Who raised the interaction.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InteractionOrigin {
    /// The provider asked, through its own request id.
    Provider {
        /// The provider that asked.
        provider_id: String,
        /// The provider's own request id.
        provider_request_id: String,
    },
    /// A plugin asked.
    Plugin {
        /// The plugin.
        plugin_id: String,
        /// The plugin's renderer.
        renderer_id: String,
    },
}

impl Default for InteractionOrigin {
    fn default() -> Self {
        InteractionOrigin::Provider {
            provider_id: String::new(),
            provider_request_id: String::new(),
        }
    }
}

/// The body of an interaction, in the contract's discriminated shape.
///
/// Kept as JSON for anything loom does not project: the contract's payload
/// union has ten variants (an approval over five different subjects, a
/// question, a plugin body), and re-encoding them here would be a second
/// schema to keep in step with the first. What loom *does* do is decide the
/// `kind` — because resolution has to be validated against it — and keep the
/// body exact for the client to render.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InteractionPayload {
    /// Which payload variant this is.
    pub kind: InteractionKind,
    /// The body, verbatim, including whatever discriminator it carries.
    pub body: serde_json::Value,
}

impl InteractionPayload {
    /// Wraps a body under an explicit kind.
    pub fn new(kind: InteractionKind, body: serde_json::Value) -> Self {
        Self { kind, body }
    }

    /// The contract `payload` value: the body with its own `kind` ensured.
    ///
    /// The contract's payload variants each carry their own `kind` const, so a
    /// body that lost it on the way in gets this interaction's kind rather than
    /// being emitted invalid.
    pub fn value(&self) -> serde_json::Value {
        match &self.body {
            serde_json::Value::Object(object) => {
                let mut object = object.clone();
                object.insert("kind".into(), self.kind.payload_kind().into());
                serde_json::Value::Object(object)
            }
            _ => serde_json::json!({ "kind": self.kind.payload_kind() }),
        }
    }
}

/// The answer to an interaction, in the contract's discriminated shape.
///
/// One struct per verb would duplicate the validation; instead every variant is
/// here and [`Resolution::matches`] decides whether it can answer a given
/// payload. The route never has to guess.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Resolution {
    /// One of the three permission decisions (`threads.resolveInteraction`).
    Decision {
        /// `allow_once`, `allow_for_session` or `deny`.
        decision: String,
        /// The permissions granted, absent for `deny`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        granted_permissions: Option<serde_json::Value>,
    },
    /// Answers to a provider's questions.
    UserAnswer {
        /// The answers, keyed by question id.
        answers: serde_json::Value,
    },
    /// A plugin's own submission.
    PluginSubmitted,
    /// An opaque value (`threads.respondToInteraction`, and the
    /// `request_answer` branch of `threads.resolveInteraction`).
    RequestAnswer {
        /// The value, verbatim.
        value: serde_json::Value,
    },
}

impl Resolution {
    /// The stable wire tag.
    pub fn kind(&self) -> &'static str {
        match self {
            Resolution::Decision { .. } => "permission_decision",
            Resolution::UserAnswer { .. } => "user_answer",
            Resolution::PluginSubmitted => "plugin_submitted",
            Resolution::RequestAnswer { .. } => "request_answer",
        }
    }

    /// Whether this resolution can answer an interaction of `kind`.
    ///
    /// The check is deliberately one-directional and explicit: a decision
    /// cannot answer a question, a question's answers cannot answer an opaque
    /// plugin request, and a typed answer never answers a kind it does not
    /// model. Refusing the mismatch is what keeps `resolve` and `respond` from
    /// silently becoming the same operation.
    ///
    /// An approval takes the **typed** decision and nothing else, because the
    /// contract's response union says so: an approval's `resolution` is one of
    /// the three decision shapes or `null`, and a `request_answer` there is
    /// rejected by the exported schema before any client sees it. The
    /// consequence is that an approval's provider-specific option is chosen by
    /// its *polarity* (`allow_once` / `allow_for_session` / `deny`) rather than
    /// named; see `crates/worker/src/acp/permission.rs` for how a polarity maps
    /// onto an ACP option and what that costs for a multi-choice request.
    pub fn answers(&self, kind: InteractionKind) -> bool {
        match (self, kind) {
            // A typed permission decision answers an approval.
            (Resolution::Decision { .. }, InteractionKind::Approval) => true,
            // Question answers answer a question.
            (Resolution::UserAnswer { .. }, InteractionKind::UserQuestion) => true,
            // A plugin submission answers a plugin request.
            (Resolution::PluginSubmitted, InteractionKind::Plugin) => true,
            // An opaque value answers anything loom cannot interpret. A plugin
            // body is opaque to loom in exactly the same way.
            (
                Resolution::RequestAnswer { .. },
                InteractionKind::Generic | InteractionKind::Plugin,
            ) => true,
            _ => false,
        }
    }

    /// The contract `resolution` value.
    pub fn value(&self) -> serde_json::Value {
        match self {
            Resolution::Decision {
                decision,
                granted_permissions,
            } => {
                let mut object = serde_json::Map::new();
                object.insert("decision".into(), decision.clone().into());
                if let Some(permissions) = granted_permissions {
                    object.insert("grantedPermissions".into(), permissions.clone());
                }
                serde_json::Value::Object(object)
            }
            Resolution::UserAnswer { answers } => serde_json::json!({
                "kind": "user_answer",
                "answers": answers,
            }),
            Resolution::PluginSubmitted => serde_json::json!({ "kind": "plugin_submitted" }),
            Resolution::RequestAnswer { value } => serde_json::json!({
                "kind": "request_answer",
                "value": value,
            }),
        }
    }
}

/// A provider's request to the user, and the record of its answer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Interaction {
    /// Identity.
    pub id: InteractionId,
    /// The thread the request belongs to.
    pub thread_id: ThreadId,
    /// Owning project, retained so cross-node realtime projection needs no lookup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<ProjectId>,
    /// The turn that raised it. The contract requires a turn id; a provider
    /// frame that arrives outside a run is recorded against the thread's most
    /// recent run, and an interaction with no run at all carries the empty
    /// string rather than a fabricated id.
    pub turn_id: String,
    /// What kind of request it is.
    pub kind: InteractionKind,
    /// Who raised it.
    pub origin: InteractionOrigin,
    /// The request body.
    pub payload: InteractionPayload,
    /// Where it is in its life.
    pub status: InteractionStatus,
    /// Why it is in that status, when there is something to say.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_reason: Option<String>,
    /// The agent's own identifier for the conversation that asked.
    ///
    /// Set when the request came from a provider that has named a session, so
    /// a client can correlate the question with the `providerThreadId` its run
    /// events carry. ACP's session id is the value; a provider that never named
    /// one leaves this `None` and the projection falls back to loom's thread
    /// id, which is the only identity such a request has.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_thread_id: Option<String>,
    /// The answer, once there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution: Option<Resolution>,
    /// When the request expires, when the provider set a limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_ms: Option<u64>,
    /// Wall-clock milliseconds when the server accepted it.
    pub created_at_ms: u64,
    /// Wall-clock milliseconds when it was settled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_at_ms: Option<u64>,
}

/// Everything needed to record an interaction.
#[derive(Clone, Debug)]
pub struct NewInteraction {
    /// The thread the request belongs to.
    pub thread_id: ThreadId,
    /// The turn that raised it.
    pub turn_id: String,
    /// What kind of request it is.
    pub kind: InteractionKind,
    /// Who raised it.
    pub origin: InteractionOrigin,
    /// The request body.
    pub payload: InteractionPayload,
    /// When the request expires, if the provider said so.
    pub expires_at_ms: Option<u64>,
    /// The agent's identifier for the conversation that asked, when there is
    /// one. See [`Interaction::provider_thread_id`].
    pub provider_thread_id: Option<String>,
    /// An identity to reuse instead of minting one.
    ///
    /// A provider repeats its request id across redeliveries; when the caller
    /// has a stable id from the provider, reusing it here is what makes a
    /// redelivered interaction the same row rather than a second question.
    pub id: Option<InteractionId>,
}

impl Interaction {
    /// Records a request, refusing an empty turn id.
    ///
    /// The turn id is required by the contract, and an interaction that cannot
    /// say which turn asked is not renderable in a timeline. Rather than store
    /// a row the client would have to guess about, the caller must supply the
    /// turn.
    pub fn create(new: NewInteraction, now_ms: u64) -> Result<Self, DomainError> {
        if new.turn_id.trim().is_empty() {
            return Err(DomainError::InvalidField {
                field: "turnId",
                reason: "an interaction belongs to a turn".into(),
            });
        }
        Ok(Self {
            id: new.id.unwrap_or_else(InteractionId::mint),
            thread_id: new.thread_id,
            project_id: None,
            turn_id: new.turn_id,
            kind: new.kind,
            origin: new.origin,
            payload: new.payload,
            status: InteractionStatus::Pending,
            status_reason: None,
            resolution: None,
            expires_at_ms: new.expires_at_ms,
            provider_thread_id: new.provider_thread_id,
            created_at_ms: now_ms,
            resolved_at_ms: None,
        })
    }

    /// Records an answer as accepted but not yet delivered.
    pub fn begin_resolution(
        &mut self,
        resolution: Resolution,
        now_ms: u64,
    ) -> Result<(), DomainError> {
        if !resolution.answers(self.kind) {
            return Err(DomainError::InvalidField {
                field: "resolution",
                reason: format!(
                    "a {} cannot answer a {} interaction",
                    resolution.kind(),
                    self.kind
                ),
            });
        }
        if self.status == InteractionStatus::Resolving
            && self.resolution.as_ref() == Some(&resolution)
        {
            return Ok(());
        }
        self.transition(InteractionStatus::Resolving, now_ms)?;
        self.resolution = Some(resolution);
        self.status_reason = None;
        Ok(())
    }

    /// Marks a previously accepted answer as delivered.
    pub fn complete_resolution(&mut self, now_ms: u64) -> Result<(), DomainError> {
        if self.status != InteractionStatus::Resolving || self.resolution.is_none() {
            return Err(DomainError::IllegalInteractionTransition {
                from: self.status,
                to: InteractionStatus::Resolved,
            });
        }
        self.settle(InteractionStatus::Resolved, None, now_ms);
        Ok(())
    }

    /// Applies an answer atomically for callers with no external delivery step.
    pub fn resolve(&mut self, resolution: Resolution, now_ms: u64) -> Result<(), DomainError> {
        self.begin_resolution(resolution, now_ms)?;
        self.complete_resolution(now_ms)
    }

    /// Records a cancellation as accepted but not yet delivered.
    pub fn begin_cancellation(
        &mut self,
        reason: Option<String>,
        now_ms: u64,
    ) -> Result<(), DomainError> {
        if self.status == InteractionStatus::Resolving
            && self.resolution.is_none()
            && self.status_reason == reason
        {
            return Ok(());
        }
        self.transition(InteractionStatus::Resolving, now_ms)?;
        self.resolution = None;
        self.status_reason = reason;
        Ok(())
    }

    /// Marks a previously accepted cancellation as delivered.
    pub fn complete_cancellation(&mut self, now_ms: u64) -> Result<(), DomainError> {
        if self.status != InteractionStatus::Resolving || self.resolution.is_some() {
            return Err(DomainError::IllegalInteractionTransition {
                from: self.status,
                to: InteractionStatus::Interrupted,
            });
        }
        let reason = self.status_reason.take();
        self.settle(InteractionStatus::Interrupted, reason, now_ms);
        Ok(())
    }

    /// Settles the interaction without an answer.
    ///
    /// Run teardown may interrupt either a pending request or an answer whose
    /// delivery was still in flight.
    pub fn cancel(&mut self, reason: Option<String>, now_ms: u64) -> Result<(), DomainError> {
        if self.status.is_terminal() {
            return Err(DomainError::IllegalInteractionTransition {
                from: self.status,
                to: InteractionStatus::Interrupted,
            });
        }
        self.settle(InteractionStatus::Interrupted, reason, now_ms);
        Ok(())
    }

    /// Whether the interaction has outlived its provider's limit.
    pub fn is_expired(&self, now_ms: u64) -> bool {
        self.expires_at_ms
            .is_some_and(|expires_at| expires_at <= now_ms)
    }

    fn transition(&mut self, to: InteractionStatus, now_ms: u64) -> Result<(), DomainError> {
        if !self.status.is_open() {
            return Err(DomainError::IllegalInteractionTransition {
                from: self.status,
                to,
            });
        }
        self.status = to;
        let _ = now_ms;
        Ok(())
    }

    fn settle(&mut self, to: InteractionStatus, reason: Option<String>, now_ms: u64) {
        self.status = to;
        self.status_reason = reason;
        self.resolved_at_ms = Some(now_ms);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn interaction(kind: InteractionKind) -> Interaction {
        Interaction::create(
            NewInteraction {
                thread_id: ThreadId::mint(),
                turn_id: "run_01M27Y6Q0J8V4W2C7K5N3P1R9Z".into(),
                kind,
                origin: InteractionOrigin::Provider {
                    provider_id: "pi".into(),
                    provider_request_id: "req-1".into(),
                },
                payload: InteractionPayload::new(kind, serde_json::json!({})),
                expires_at_ms: None,
                provider_thread_id: Some("acp-session-1".into()),
                id: None,
            },
            1,
        )
        .unwrap()
    }

    #[test]
    fn an_interaction_starts_pending_and_open() {
        let interaction = interaction(InteractionKind::Approval);
        assert_eq!(interaction.status, InteractionStatus::Pending);
        assert!(interaction.status.is_open());
        assert!(!interaction.status.is_terminal());
        assert_eq!(interaction.resolved_at_ms, None);
    }

    #[test]
    fn an_interaction_must_name_a_turn() {
        assert!(matches!(
            Interaction::create(
                NewInteraction {
                    thread_id: ThreadId::mint(),
                    turn_id: "  ".into(),
                    kind: InteractionKind::Generic,
                    origin: InteractionOrigin::default(),
                    payload: InteractionPayload::new(
                        InteractionKind::Generic,
                        serde_json::json!({})
                    ),
                    expires_at_ms: None,
                    provider_thread_id: None,
                    id: None,
                },
                1
            ),
            Err(DomainError::InvalidField {
                field: "turnId",
                ..
            })
        ));
    }

    #[test]
    fn a_decision_answers_an_approval_and_nothing_else() {
        let decision = Resolution::Decision {
            decision: "deny".into(),
            granted_permissions: None,
        };
        assert!(decision.answers(InteractionKind::Approval));
        assert!(!decision.answers(InteractionKind::UserQuestion));
        assert!(!decision.answers(InteractionKind::Generic));
        assert!(!decision.answers(InteractionKind::Plugin));
    }

    #[test]
    fn each_typed_resolution_answers_only_its_own_kind() {
        let answers = Resolution::UserAnswer {
            answers: serde_json::json!({ "q1": { "selected": ["a"] } }),
        };
        assert!(answers.answers(InteractionKind::UserQuestion));
        assert!(!answers.answers(InteractionKind::Approval));
        assert!(!answers.answers(InteractionKind::Generic));

        let plugin = Resolution::PluginSubmitted;
        assert!(plugin.answers(InteractionKind::Plugin));
        assert!(!plugin.answers(InteractionKind::Generic));
        assert!(!plugin.answers(InteractionKind::UserQuestion));

        // An opaque value answers only what loom cannot interpret. A plugin
        // body is opaque to loom too, so it is accepted there.
        let opaque = Resolution::RequestAnswer {
            value: serde_json::json!("yes"),
        };
        assert!(opaque.answers(InteractionKind::Generic));
        assert!(opaque.answers(InteractionKind::Plugin));
        assert!(!opaque.answers(InteractionKind::UserQuestion));
    }

    #[test]
    fn an_opaque_value_does_not_answer_an_approval() {
        // The contract's response union for an approval admits only the three
        // decision shapes or `null`, so an opaque answer there would be a
        // response the exported schema rejects. Keeping it out here means the
        // refusal happens in the domain rather than in a client.
        let opaque = Resolution::RequestAnswer {
            value: serde_json::json!({ "optionId": "choice-1" }),
        };
        assert!(!opaque.answers(InteractionKind::Approval));
        assert!(opaque.answers(InteractionKind::Generic));
        assert!(opaque.answers(InteractionKind::Plugin));
        assert!(!opaque.answers(InteractionKind::UserQuestion));
        // The typed verb is the only one, which is what keeps `resolve` and
        // `respond` from being the same operation.
        assert!(Resolution::Decision {
            decision: "deny".into(),
            granted_permissions: None,
        }
        .answers(InteractionKind::Approval));
    }

    #[test]
    fn an_interaction_records_the_agents_conversation_id() {
        let approval = interaction(InteractionKind::Approval);
        assert_eq!(
            approval.provider_thread_id.as_deref(),
            Some("acp-session-1")
        );
    }

    #[test]
    fn resolution_stays_retryable_until_delivery_completes() {
        let mut approval = interaction(InteractionKind::Approval);
        let decision = Resolution::Decision {
            decision: "deny".into(),
            granted_permissions: None,
        };
        approval.begin_resolution(decision.clone(), 6).unwrap();
        assert_eq!(approval.status, InteractionStatus::Resolving);
        assert_eq!(approval.resolution.as_ref(), Some(&decision));
        assert_eq!(approval.resolved_at_ms, None);
        approval
            .begin_resolution(decision, 7)
            .expect("an identical delivery retry is idempotent");
        approval.complete_resolution(8).unwrap();
        assert_eq!(approval.status, InteractionStatus::Resolved);
        assert_eq!(approval.resolved_at_ms, Some(8));
    }

    #[test]
    fn cancellation_stays_retryable_until_delivery_completes() {
        let mut approval = interaction(InteractionKind::Approval);
        let reason = Some("cancelled by a client".to_owned());
        approval.begin_cancellation(reason.clone(), 6).unwrap();
        assert_eq!(approval.status, InteractionStatus::Resolving);
        assert_eq!(approval.status_reason, reason);
        assert_eq!(approval.resolution, None);
        approval
            .begin_cancellation(Some("cancelled by a client".to_owned()), 7)
            .expect("an identical cancellation retry is idempotent");
        approval.complete_cancellation(8).unwrap();
        assert_eq!(approval.status, InteractionStatus::Interrupted);
        assert_eq!(
            approval.status_reason.as_deref(),
            Some("cancelled by a client")
        );
        assert_eq!(approval.resolved_at_ms, Some(8));
    }

    #[test]
    fn resolving_records_the_answer_and_settles_it() {
        let mut approval = interaction(InteractionKind::Approval);
        approval
            .resolve(
                Resolution::Decision {
                    decision: "allow_once".into(),
                    granted_permissions: Some(serde_json::json!({
                        "network": { "enabled": true },
                        "fileSystem": { "read": [], "write": [] },
                    })),
                },
                7,
            )
            .unwrap();
        assert_eq!(approval.status, InteractionStatus::Resolved);
        assert_eq!(approval.resolved_at_ms, Some(7));
        assert!(approval.resolution.is_some());
    }

    #[test]
    fn a_mismatched_resolution_is_refused_without_mutating() {
        let mut approval = interaction(InteractionKind::Approval);
        let before = approval.clone();
        assert!(matches!(
            approval.resolve(
                Resolution::UserAnswer {
                    answers: serde_json::json!({})
                },
                7
            ),
            Err(DomainError::InvalidField {
                field: "resolution",
                ..
            })
        ));
        assert_eq!(approval, before);
    }

    #[test]
    fn a_settled_interaction_refuses_a_second_answer() {
        let mut approval = interaction(InteractionKind::Approval);
        let decision = Resolution::Decision {
            decision: "deny".into(),
            granted_permissions: None,
        };
        approval.resolve(decision.clone(), 7).unwrap();
        assert!(matches!(
            approval.resolve(decision, 8),
            Err(DomainError::IllegalInteractionTransition {
                from: InteractionStatus::Resolved,
                to: InteractionStatus::Resolving,
            })
        ));
        assert_eq!(approval.resolved_at_ms, Some(7));
    }

    #[test]
    fn cancelling_settles_without_an_answer_from_every_open_status() {
        for status in [InteractionStatus::Pending, InteractionStatus::Resolving] {
            let mut interaction = interaction(InteractionKind::Generic);
            interaction.status = status;
            interaction.cancel(Some("run stopped".into()), 9).unwrap();
            assert_eq!(interaction.status, InteractionStatus::Interrupted);
            assert_eq!(interaction.status_reason.as_deref(), Some("run stopped"));
            assert_eq!(interaction.resolution, None);
            assert_eq!(interaction.resolved_at_ms, Some(9));
        }
    }

    #[test]
    fn a_terminal_interaction_cannot_be_cancelled() {
        for status in [InteractionStatus::Resolved, InteractionStatus::Interrupted] {
            let mut interaction = interaction(InteractionKind::Generic);
            interaction.status = status;
            assert!(matches!(
                interaction.cancel(None, 9),
                Err(DomainError::IllegalInteractionTransition { .. })
            ));
        }
    }

    #[test]
    fn an_expiry_is_reported_rather_than_enforced_by_the_type() {
        let mut approval = interaction(InteractionKind::Approval);
        approval.expires_at_ms = Some(50);
        assert!(!approval.is_expired(49));
        assert!(approval.is_expired(50));
        assert!(approval.status.is_open(), "expiry is not a status change");
    }

    #[test]
    fn a_payload_body_keeps_its_own_shape_and_gains_a_kind() {
        let payload = InteractionPayload::new(
            InteractionKind::UserQuestion,
            serde_json::json!({ "questions": [] }),
        );
        let value = payload.value();
        assert_eq!(value["kind"], "user_question");
        assert_eq!(value["questions"], serde_json::json!([]));
    }

    #[test]
    fn every_resolution_serializes_to_its_contract_shape() {
        let decision = Resolution::Decision {
            decision: "allow_once".into(),
            granted_permissions: None,
        };
        assert_eq!(decision.value()["decision"], "allow_once");
        assert!(decision.value().get("grantedPermissions").is_none());

        let answer = Resolution::UserAnswer {
            answers: serde_json::json!({ "q1": { "selected": ["a"] } }),
        };
        assert_eq!(answer.value()["kind"], "user_answer");

        assert_eq!(
            Resolution::PluginSubmitted.value()["kind"],
            "plugin_submitted"
        );
        assert_eq!(
            Resolution::RequestAnswer {
                value: serde_json::json!(7)
            }
            .value()["value"],
            7
        );
    }
}
