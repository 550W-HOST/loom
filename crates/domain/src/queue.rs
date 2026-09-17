//! Queued messages: input a client left for a thread that was not ready for it.
//!
//! A *queued message* is what a user typed while a thread was busy (or scheduled
//! for later), and it is deliberately a durable entity rather than a pending
//! HTTP request: the client that created it disconnects, the server restarts,
//! and the message must still be there to send when the thread comes back. That
//! is why it lives in the entity view alongside threads and environments
//! (see `docs/domain-persistence.md`) instead of in a handler's memory.
//!
//! # The state machine
//!
//! ```text
//!   queued ──send──▶ sent        (terminal)
//!      │
//!      └──cancel──▶ cancelled   (terminal)
//! ```
//!
//! `queued` is the only mutable status, and `sent` and `cancelled` are both
//! terminal — a sent message is a turn's origin, and un-sending it would mean
//! rewriting a turn the provider may already have executed. Both transitions
//! are refused from a terminal status rather than being silently idempotent,
//! because a second `send` is a lost-claim race the caller has to see.
//!
//! # What it is not
//!
//! It is not a message: nothing here appends to a conversation. Sending is the
//! caller's job — [`crate::Thread::post_message`] is what a send ultimately
//! does — and this type only records that the input existed, where it came
//! from and what happened to it.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::error::DomainError;
use crate::id::{ProjectId, QueuedMessageId, ThreadId};
use crate::thread::ReasoningLevel;

/// A queued message's position in its own small lifecycle.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueuedMessageStatus {
    /// Waiting to be sent.
    #[default]
    Queued,
    /// Delivered to the thread as a user turn.
    Sent,
    /// Withdrawn before it was sent.
    Cancelled,
}

impl QueuedMessageStatus {
    /// Whether a message in this status can still be sent or cancelled.
    pub fn is_open(self) -> bool {
        matches!(self, QueuedMessageStatus::Queued)
    }

    /// The stable wire token.
    pub fn as_str(self) -> &'static str {
        match self {
            QueuedMessageStatus::Queued => "queued",
            QueuedMessageStatus::Sent => "sent",
            QueuedMessageStatus::Cancelled => "cancelled",
        }
    }
}

impl fmt::Display for QueuedMessageStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why a message is in the queue at all.
///
/// The contract types this as the message's `payload` and the two variants are
/// genuinely different rows in a client: an inline message is something the
/// user composed and deferred, a retry is a re-run of a turn that already
/// happened. Collapsing them would lose the attempt number and the reason.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum QueuedMessagePayload {
    /// The client supplied the content directly.
    #[default]
    Inline,
    /// The message re-runs a previous turn (`threads.retry` with `sendAt`).
    Retry {
        /// The turn request being retried.
        retry_of_turn_request_id: String,
        /// How many attempts this thread's turn has had, this one included.
        attempt: u64,
        /// Why the retry was asked for, verbatim.
        reason: String,
    },
}

impl QueuedMessagePayload {
    /// The stable wire tag.
    pub fn kind(&self) -> &'static str {
        match self {
            QueuedMessagePayload::Inline => "inline",
            QueuedMessagePayload::Retry { .. } => "retry",
        }
    }
}

impl fmt::Display for QueuedMessagePayload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.kind())
    }
}

/// Who put the message in the queue.
///
/// Deliberately not [`crate::MessageRole`]: a queued message has no
/// conversation role until it is sent, and the contract's `initiator` is about
/// authorship of the *queue entry*. `assistant` is not an initiator here, which
/// is the difference that matters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueuedMessageInitiator {
    /// The human, or the UI acting for them.
    #[default]
    User,
    /// An agent, for example a delegated thread answering its parent.
    Agent,
    /// The server itself.
    System,
}

impl QueuedMessageInitiator {
    /// The stable wire token.
    pub fn as_str(self) -> &'static str {
        match self {
            QueuedMessageInitiator::User => "user",
            QueuedMessageInitiator::Agent => "agent",
            QueuedMessageInitiator::System => "system",
        }
    }
}

impl fmt::Display for QueuedMessageInitiator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The service tier the message asks for (`serviceTierSchema`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ServiceTier {
    /// The provider's normal tier.
    #[default]
    Default,
    /// The provider's fast tier.
    Fast,
}

impl ServiceTier {
    /// The stable wire token.
    pub fn as_str(self) -> &'static str {
        match self {
            ServiceTier::Default => "default",
            ServiceTier::Fast => "fast",
        }
    }
}

impl fmt::Display for ServiceTier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Input a client left for a thread that was not ready to take it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueuedMessage {
    /// Identity.
    pub id: QueuedMessageId,
    /// The thread the message will be delivered to.
    pub thread_id: ThreadId,
    /// Owning project, retained so cross-node realtime projection needs no lookup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<ProjectId>,
    /// The thread it was sent *from*, when it came from another conversation.
    pub sender_thread_id: Option<ThreadId>,
    /// Who queued it.
    pub initiator: QueuedMessageInitiator,
    /// The prompt text.
    ///
    /// loom stores the text and projects it back into the contract's content
    /// array. A block the execution plane cannot deliver (an image, a file) is
    /// refused at the route rather than stored and dropped — see
    /// `docs/contract.md`.
    pub text: String,
    /// The model the resulting turn should use, when the client named one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The reasoning level the resulting turn should use.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_level: Option<ReasoningLevel>,
    /// The permission mode the resulting turn should run under.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<String>,
    /// The service tier the resulting turn should use.
    #[serde(default)]
    pub service_tier: ServiceTier,
    /// Whether the client asked for this message to be grouped with the next.
    #[serde(default)]
    pub group_with_next: bool,
    /// Stable fractional ordering key within this thread's queue.
    ///
    /// Older snapshots did not have an order key; the server assigns one when
    /// restoring those rows before accepting a reorder.
    #[serde(default)]
    pub sort_key: String,
    /// When the client wants it sent; `None` means as soon as the thread allows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub send_at: Option<u64>,
    /// Why it is queued.
    pub payload: QueuedMessagePayload,
    /// Where it is in its lifecycle.
    pub status: QueuedMessageStatus,
    /// Why a send failed, when one did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_reason: Option<String>,
    /// Wall-clock milliseconds when the server accepted it.
    pub created_at_ms: u64,
    /// Wall-clock milliseconds of the last accepted mutation.
    pub updated_at_ms: u64,
}

/// Everything needed to queue a message.
#[derive(Clone, Debug)]
pub struct NewQueuedMessage {
    /// The thread the message will be delivered to.
    pub thread_id: ThreadId,
    /// The thread it came from, if any.
    pub sender_thread_id: Option<ThreadId>,
    /// Who queued it.
    pub initiator: QueuedMessageInitiator,
    /// The prompt text.
    pub text: String,
    /// The requested model, if any.
    pub model: Option<String>,
    /// The requested reasoning level, if any.
    pub reasoning_level: Option<ReasoningLevel>,
    /// The requested permission mode, if any.
    pub permission_mode: Option<String>,
    /// The requested service tier.
    pub service_tier: ServiceTier,
    /// Whether to group with the next message.
    pub group_with_next: bool,
    /// When to send it, if scheduled.
    pub send_at: Option<u64>,
    /// Why it is queued.
    pub payload: QueuedMessagePayload,
}

impl QueuedMessage {
    /// Queues a message, rejecting an empty prompt.
    ///
    /// An empty prompt is rejected here rather than at send time so the client
    /// learns immediately; a row that can never be delivered is worse than a
    /// refusal.
    pub fn create(new: NewQueuedMessage, now_ms: u64) -> Result<Self, DomainError> {
        let text = new.text.trim().to_owned();
        if text.is_empty() {
            return Err(DomainError::InvalidField {
                field: "input",
                reason: "must contain text".into(),
            });
        }
        Ok(Self {
            id: QueuedMessageId::mint(),
            thread_id: new.thread_id,
            project_id: None,
            sender_thread_id: new.sender_thread_id,
            initiator: new.initiator,
            text: new.text,
            model: new.model,
            reasoning_level: new.reasoning_level,
            permission_mode: new.permission_mode,
            service_tier: new.service_tier,
            group_with_next: new.group_with_next,
            sort_key: String::new(),
            send_at: new.send_at,
            payload: new.payload,
            status: QueuedMessageStatus::Queued,
            failure_reason: None,
            created_at_ms: now_ms,
            updated_at_ms: now_ms,
        })
    }

    /// Whether this message may be sent at `now_ms`.
    ///
    /// A scheduled message that is not due yet is not sendable, which is what
    /// makes a `send` of one a distinct outcome from a delivery.
    pub fn is_due(&self, now_ms: u64) -> bool {
        match self.send_at {
            None => true,
            Some(send_at) => send_at <= now_ms,
        }
    }

    /// Moves the message to `sent`.
    ///
    /// Terminal: a second send is a claim race, not a no-op.
    pub fn mark_sent(&mut self, now_ms: u64) -> Result<(), DomainError> {
        self.transition(QueuedMessageStatus::Sent, now_ms)
    }

    /// Moves the message to `cancelled`.
    pub fn cancel(&mut self, now_ms: u64) -> Result<(), DomainError> {
        self.transition(QueuedMessageStatus::Cancelled, now_ms)
    }

    /// Records why a send failed, leaving the message queued.
    ///
    /// A failed send is recoverable: the message stays in the queue with the
    /// reason attached, so a client can see why and retry.
    pub fn record_failure(&mut self, reason: String, now_ms: u64) {
        self.failure_reason = Some(reason);
        self.updated_at_ms = now_ms;
    }

    /// Clears a recorded failure.
    pub fn clear_failure(&mut self, now_ms: u64) {
        if self.failure_reason.take().is_some() {
            self.updated_at_ms = now_ms;
        }
    }

    /// Replaces the prompt text while the row is still queued.
    pub fn update_text(&mut self, text: String, now_ms: u64) -> Result<(), DomainError> {
        if !self.status.is_open() {
            return Err(DomainError::IllegalQueuedMessageTransition {
                from: self.status,
                to: self.status,
            });
        }
        if text.trim().is_empty() {
            return Err(DomainError::InvalidField {
                field: "input",
                reason: "must contain text".into(),
            });
        }
        self.text = text;
        self.updated_at_ms = now_ms.max(self.updated_at_ms.saturating_add(1));
        Ok(())
    }

    /// Changes the durable queue order key.
    pub fn set_sort_key(&mut self, sort_key: String, now_ms: u64) {
        self.sort_key = sort_key;
        self.updated_at_ms = now_ms.max(self.updated_at_ms.saturating_add(1));
    }

    /// Changes the edge that groups this row with the following row.
    pub fn set_group_with_next(&mut self, group_with_next: bool, now_ms: u64) {
        if self.group_with_next != group_with_next {
            self.group_with_next = group_with_next;
            self.updated_at_ms = now_ms.max(self.updated_at_ms.saturating_add(1));
        }
    }

    fn transition(&mut self, to: QueuedMessageStatus, now_ms: u64) -> Result<(), DomainError> {
        if !self.status.is_open() {
            return Err(DomainError::IllegalQueuedMessageTransition {
                from: self.status,
                to,
            });
        }
        self.status = to;
        self.updated_at_ms = now_ms;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new(thread_id: ThreadId, text: &str) -> NewQueuedMessage {
        NewQueuedMessage {
            thread_id,
            sender_thread_id: None,
            initiator: QueuedMessageInitiator::User,
            text: text.into(),
            model: None,
            reasoning_level: None,
            permission_mode: None,
            service_tier: ServiceTier::Default,
            group_with_next: false,
            send_at: None,
            payload: QueuedMessagePayload::Inline,
        }
    }

    #[test]
    fn a_queued_message_starts_open() {
        let message = QueuedMessage::create(new(ThreadId::mint(), "later"), 1).unwrap();
        assert_eq!(message.status, QueuedMessageStatus::Queued);
        assert!(message.is_due(1));
        assert_eq!(message.failure_reason, None);
    }

    #[test]
    fn an_empty_prompt_is_rejected() {
        assert!(matches!(
            QueuedMessage::create(new(ThreadId::mint(), "   "), 1),
            Err(DomainError::InvalidField { field: "input", .. })
        ));
    }

    #[test]
    fn a_scheduled_message_is_not_due_until_its_time() {
        let mut new = new(ThreadId::mint(), "at midnight");
        new.send_at = Some(50);
        let message = QueuedMessage::create(new, 1).unwrap();
        assert!(!message.is_due(49));
        assert!(message.is_due(50));
    }

    #[test]
    fn sending_and_cancelling_are_terminal() {
        for terminal in [
            QueuedMessage::mark_sent as fn(&mut QueuedMessage, u64) -> Result<(), DomainError>,
            QueuedMessage::cancel,
        ] {
            let mut message = QueuedMessage::create(new(ThreadId::mint(), "x"), 1).unwrap();
            terminal(&mut message, 2).unwrap();
            assert!(!message.status.is_open());
            assert!(matches!(
                terminal(&mut message, 3),
                Err(DomainError::IllegalQueuedMessageTransition { .. })
            ));
            assert_eq!(
                message.updated_at_ms, 2,
                "a refused transition must not mutate"
            );
        }
    }

    #[test]
    fn the_four_transitions_are_the_whole_state_machine() {
        let open = QueuedMessageStatus::Queued;
        assert!(open.is_open());
        assert!(!QueuedMessageStatus::Sent.is_open());
        assert!(!QueuedMessageStatus::Cancelled.is_open());

        // Every legal transition out of every status, enumerated.
        for (status, to, legal) in [
            (open, QueuedMessageStatus::Sent, true),
            (open, QueuedMessageStatus::Cancelled, true),
            (QueuedMessageStatus::Sent, QueuedMessageStatus::Sent, false),
            (
                QueuedMessageStatus::Sent,
                QueuedMessageStatus::Cancelled,
                false,
            ),
            (
                QueuedMessageStatus::Cancelled,
                QueuedMessageStatus::Sent,
                false,
            ),
            (
                QueuedMessageStatus::Cancelled,
                QueuedMessageStatus::Cancelled,
                false,
            ),
        ] {
            let mut message = QueuedMessage::create(new(ThreadId::mint(), "x"), 1).unwrap();
            message.status = status;
            let result = message.transition(to, 2);
            assert_eq!(
                result.is_ok(),
                legal,
                "{status} -> {to} should {}be legal",
                if legal { "" } else { "not " }
            );
            assert_eq!(message.status, if legal { to } else { status });
        }
    }

    #[test]
    fn a_failure_leaves_the_message_queued_for_a_retry() {
        let mut message = QueuedMessage::create(new(ThreadId::mint(), "x"), 1).unwrap();
        message.record_failure("host offline".into(), 2);
        assert_eq!(message.status, QueuedMessageStatus::Queued);
        assert_eq!(message.failure_reason.as_deref(), Some("host offline"));

        message.clear_failure(3);
        assert_eq!(message.failure_reason, None);
        message.mark_sent(4).unwrap();
    }

    #[test]
    fn the_payload_and_initiator_tags_are_the_contract_spelling() {
        assert_eq!(QueuedMessagePayload::Inline.kind(), "inline");
        assert_eq!(
            QueuedMessagePayload::Retry {
                retry_of_turn_request_id: "creq_23456789ab".into(),
                attempt: 2,
                reason: "timed out".into(),
            }
            .kind(),
            "retry"
        );
        assert_eq!(QueuedMessageInitiator::Agent.as_str(), "agent");
        assert_eq!(ServiceTier::Fast.as_str(), "fast");
    }
}
