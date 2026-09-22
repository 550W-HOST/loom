//! Queued-message delivery: turning a stored prompt into a turn.
//!
//! A queued message is only half a feature until something delivers it. This
//! module owns that half, and it is deliberately the *only* place a queued
//! message becomes a message: `threads.sendQueuedMessage` and the automatic
//! drain both funnel through [`AppState::deliver_queued_message`], so a manual
//! send and an automatic one cannot diverge in what they publish or in what
//! happens when the thread is not ready.
//!
//! # Why a manual send can still not deliver
//!
//! `threads.sendQueuedMessage` has two contract branches and they are not two
//! spellings of one thing:
//!
//! * `delivery: "sent"` — the message became a turn now;
//! * `delivery: "queued"` — it is still in the queue, with a `waitingOn` reason.
//!
//! The second is returned when the thread is busy, when the client scheduled
//! the message for later, or when the last attempt failed. Answering `sent`
//! for a message that did not become a turn is the lie the contract's two
//! branches exist to prevent, and it is why a send of a scheduled-but-not-yet-due
//! message is `queued` rather than an error.
//!
//! # Automatic drain
//!
//! [`crate::state::AppState::drain_thread_queue`] runs after a run reaches a
//! terminal state and from the reconciler. It is the mechanism that makes
//! "queue while busy" work at all: without it a message queued during a turn
//! would sit there until a human pressed send, which is not what the client
//! that queued it asked for.

use loom_domain::{DomainEvent, MessageRole, QueuedMessage, QueuedMessageStatus, ThreadId};
use loom_relay::now_ms;

use crate::state::AppState;

/// What a delivery attempt did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeliveryOutcome {
    /// The message was appended to the thread and its run dispatched.
    Sent(Box<QueuedMessage>),
    /// The thread cannot take the message yet; it stays queued.
    StillQueued {
        /// The message, with any recorded failure cleared.
        message: Box<QueuedMessage>,
        /// Why it is still queued.
        waiting_on: WaitingOn,
    },
    /// The delivery was attempted and failed; the message stays queued with
    /// the reason recorded.
    Failed {
        /// The message, with the failure recorded.
        message: Box<QueuedMessage>,
        /// Why the delivery failed.
        reason: String,
    },
    /// The message is not in the queue (unknown id, or already settled).
    Unknown,
}

/// Why a queued message has not been delivered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WaitingOn {
    /// The thread has a turn in flight.
    ThreadBusy,
    /// The client scheduled it for later.
    Time,
}

impl WaitingOn {
    /// The contract's `waitingOn` value.
    pub fn value(self) -> serde_json::Value {
        match self {
            WaitingOn::ThreadBusy => serde_json::json!({ "kind": "thread-busy" }),
            WaitingOn::Time => serde_json::json!({ "kind": "time" }),
        }
    }

    /// The stable wire token.
    pub fn kind(self) -> &'static str {
        match self {
            WaitingOn::ThreadBusy => "thread-busy",
            WaitingOn::Time => "time",
        }
    }
}

impl AppState {
    /// Delivers one queued message if the thread allows it.
    ///
    /// `force` distinguishes the two callers: a client pressing send
    /// (`threads.sendQueuedMessage`) is allowed to deliver a message whose
    /// `sendAt` is still in the future — it is asking for exactly that — while
    /// the automatic drain may not, because delivering early would ignore a
    /// schedule the client set.
    ///
    /// A busy thread is never overridden. `steer` is the contract's mechanism
    /// for injecting into a running turn, and loom's provider protocol cannot
    /// express it (there is no steer frame), so a busy send stays queued rather
    /// than silently becoming a second concurrent turn.
    pub fn deliver_queued_message(
        &self,
        message_id: &loom_domain::QueuedMessageId,
        force: bool,
        now: u64,
    ) -> DeliveryOutcome {
        let Some(message) = self.registry.queued_message(message_id) else {
            return DeliveryOutcome::Unknown;
        };
        if message.status != QueuedMessageStatus::Queued {
            return DeliveryOutcome::Unknown;
        }

        let Some(thread) = self.registry.thread(&message.thread_id) else {
            return DeliveryOutcome::Unknown;
        };
        if !force && !message.is_due(now) {
            return DeliveryOutcome::StillQueued {
                message: Box::new(message),
                waiting_on: WaitingOn::Time,
            };
        }
        if thread.status.is_archived() {
            return self.fail_queued_message(message, "the thread is archived".into(), now);
        }
        if !thread.status.accepts_work() {
            return DeliveryOutcome::StillQueued {
                message: Box::new(message),
                waiting_on: WaitingOn::ThreadBusy,
            };
        }

        // Append the prompt through the same registry path every other user
        // turn takes, so the timeline, the status change and the run lifecycle
        // are identical to a `threads.send`.
        let events = match self.registry.post_message(
            &thread.id,
            MessageRole::User,
            message.text.clone(),
            now,
        ) {
            Ok(events) => events,
            Err(error) => {
                return self.fail_queued_message(message, error.to_string(), now);
            }
        };
        for event in &events {
            let _ = self.publish_domain_event(event);
        }

        let started = events
            .iter()
            .any(|event| matches!(event, DomainEvent::ThreadStatusChanged { .. }));
        let (sent, event) = match self.registry.mark_queued_message_sent(&message.id, now) {
            Ok((sent, event)) => (sent, event),
            Err(error) => {
                // The turn is already running; failing the *queue row* here
                // would describe the wrong thing. Report the send as failed so
                // the caller can retry, and leave the message queued.
                let _ = error;
                return self.fail_queued_message(
                    message,
                    "the queue row could not be marked sent".into(),
                    now,
                );
            }
        };
        let _ = self.publish_domain_event(&event);
        if started {
            if let Some(thread) = self.registry.thread(&thread.id) {
                self.dispatch_thread(&thread, &sent.text);
            }
        }
        DeliveryOutcome::Sent(Box::new(sent))
    }

    /// Records why a queued message could not be delivered, leaving it queued.
    fn fail_queued_message(
        &self,
        message: QueuedMessage,
        reason: String,
        now: u64,
    ) -> DeliveryOutcome {
        match self
            .registry
            .record_queued_message_failure(&message.id, reason.clone(), now)
        {
            Ok((message, event)) => {
                let _ = self.publish_domain_event(&event);
                DeliveryOutcome::Failed {
                    message: Box::new(message),
                    reason,
                }
            }
            Err(_) => DeliveryOutcome::Unknown,
        }
    }

    /// Delivers every due message a thread has queued, in order, stopping at
    /// the first one that cannot be sent.
    ///
    /// Stopping matters: the queue is ordered, and skipping a blocked message
    /// to send a later one would reorder the conversation against the client's
    /// arrangement. Returns how many were sent.
    ///
    /// Called after a run reaches a terminal state and from the reconciler, so
    /// a message queued during a turn is delivered by a state change rather
    /// than by another client action.
    pub fn drain_thread_queue(&self, thread_id: &ThreadId) -> usize {
        let now = now_ms();
        let queued = self.registry.queued_messages_for(Some(thread_id));
        let mut sent = 0;
        for message in queued {
            if message.status != QueuedMessageStatus::Queued {
                continue;
            }
            match self.deliver_queued_message(&message.id, false, now) {
                DeliveryOutcome::Sent(_) => sent += 1,
                // A scheduled message that is not due yet blocks the ones
                // behind it, for the ordering reason above.
                _ => break,
            }
        }
        sent
    }

    /// Delivers the head of every thread queue that has a due message.
    ///
    /// The grouping by thread is intentional. Looking at all rows globally and
    /// attempting each due row would let a later message bypass an earlier
    /// scheduled one in the same thread. `drain_thread_queue` owns the
    /// per-thread FIFO check, so the clock sweep uses it as well as the
    /// terminal-run path.
    pub fn drain_due_queued_messages(&self) -> usize {
        let now = now_ms();
        let mut thread_ids = Vec::new();
        for message in self.registry.queued_messages() {
            if message.status != QueuedMessageStatus::Queued
                || !message.is_due(now)
                || thread_ids.contains(&message.thread_id)
            {
                continue;
            }
            thread_ids.push(message.thread_id);
        }

        thread_ids
            .into_iter()
            .map(|thread_id| self.drain_thread_queue(&thread_id))
            .sum()
    }
}
