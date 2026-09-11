//! Threads and their lifecycle.
//!
//! A thread is the unit of work: one conversation with a provider, carrying a
//! lifecycle status and an optional parent for delegation. The status machine
//! is defined once, in [`ThreadStatus::transition`], and every mutation goes
//! through it — there is no other way to change `Thread::status`.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::error::DomainError;
use crate::event::DomainEvent;
use crate::id::{EnvironmentId, MessageId, ProjectId, RunId, ThreadId};

/// Where a thread is in its life.
///
/// | status | meaning |
/// | --- | --- |
/// | `idle` | Nothing is running. The thread is provisioned (or not yet provisioned) and ready to accept work. |
/// | `working` | A run is in flight: the provider is executing a turn for this thread. |
/// | `waiting` | A run is paused awaiting external input (a permission decision, a clarifying answer). No compute is consumed. |
/// | `error` | The last run failed and the failure was not recoverable in place. The thread is inert until retried or archived. |
/// | `archived` | Terminal for normal use. Hidden from default lists, read-only, no run may start. |
///
/// The legal transitions, and the trigger that causes each, are the table in
/// [`ThreadStatus::transition`]. `archive` is legal from every non-archived
/// status; `unarchive` returns the thread to `idle` (the status before archival
/// is not retained).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadStatus {
    /// Nothing running; ready for work.
    Idle,
    /// A provider run is in flight.
    Working,
    /// A run is paused awaiting external input.
    Waiting,
    /// The last run failed; inert until retried.
    Error,
    /// Read-only and hidden from default lists.
    Archived,
}

impl ThreadStatus {
    /// Every status, in lifecycle order.
    pub const ALL: [ThreadStatus; 5] = [
        ThreadStatus::Idle,
        ThreadStatus::Working,
        ThreadStatus::Waiting,
        ThreadStatus::Error,
        ThreadStatus::Archived,
    ];

    /// Applies a trigger, returning the next status or `None` when the
    /// transition is illegal from `self`.
    ///
    /// This is the single source of truth for the state machine; a new
    /// lifecycle rule belongs here and nowhere else.
    pub fn transition(self, trigger: ThreadTrigger) -> Option<ThreadStatus> {
        use ThreadStatus::{Archived, Error, Idle, Waiting, Working};
        use ThreadTrigger::{
            Archive, AwaitInput, InputReceived, Retry, RunCancelled, RunCompleted, RunFailed,
            RunStarted, Unarchive,
        };

        let next = match (self, trigger) {
            (Idle, RunStarted) => Working,
            (Idle, Archive) => Archived,

            (Working, RunCompleted) => Idle,
            (Working, AwaitInput) => Waiting,
            (Working, RunFailed) => Error,
            (Working, RunCancelled) => Idle,
            (Working, Archive) => Archived,

            (Waiting, InputReceived) => Working,
            (Waiting, RunFailed) => Error,
            (Waiting, RunCancelled) => Idle,
            (Waiting, Archive) => Archived,

            (Error, Retry) => Working,
            (Error, Archive) => Archived,

            (Archived, Unarchive) => Idle,

            _ => return None,
        };
        Some(next)
    }

    /// Whether the thread accepts new work in this status.
    pub fn accepts_work(self) -> bool {
        matches!(self, ThreadStatus::Idle)
    }

    /// Whether the thread is read-only.
    pub fn is_archived(self) -> bool {
        matches!(self, ThreadStatus::Archived)
    }
}

impl fmt::Display for ThreadStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            ThreadStatus::Idle => "idle",
            ThreadStatus::Working => "working",
            ThreadStatus::Waiting => "waiting",
            ThreadStatus::Error => "error",
            ThreadStatus::Archived => "archived",
        };
        f.write_str(name)
    }
}

/// Something that happens to a thread and may move its status.
///
/// Triggers are the only input to [`ThreadStatus::transition`]. A trigger that
/// has no transition from the current status is an error, not a no-op, so a
/// stale producer cannot silently corrupt the lifecycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadTrigger {
    /// A run (agent turn) began.
    RunStarted,
    /// The in-flight run finished successfully.
    RunCompleted,
    /// The in-flight run failed.
    RunFailed,
    /// The run needs input before it can continue.
    AwaitInput,
    /// The awaited input arrived.
    InputReceived,
    /// A waiting run was cancelled.
    RunCancelled,
    /// A failed thread is being retried.
    Retry,
    /// Archive the thread.
    Archive,
    /// Restore an archived thread to `idle`.
    Unarchive,
}

impl fmt::Display for ThreadTrigger {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            ThreadTrigger::RunStarted => "run_started",
            ThreadTrigger::RunCompleted => "run_completed",
            ThreadTrigger::RunFailed => "run_failed",
            ThreadTrigger::AwaitInput => "await_input",
            ThreadTrigger::InputReceived => "input_received",
            ThreadTrigger::RunCancelled => "run_cancelled",
            ThreadTrigger::Retry => "retry",
            ThreadTrigger::Archive => "archive",
            ThreadTrigger::Unarchive => "unarchive",
        };
        f.write_str(name)
    }
}

/// Who produced a [`ThreadMessage`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    /// The human (or the UI acting for them).
    #[default]
    User,
    /// The provider.
    Assistant,
    /// The server or provider wrapper.
    System,
}

/// One turn-level message in a thread's timeline.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadMessage {
    /// The message's identity.
    pub id: MessageId,
    /// The thread it belongs to.
    pub thread_id: ThreadId,
    /// Who produced it.
    pub role: MessageRole,
    /// The message body, verbatim.
    pub content: String,
    /// Wall-clock milliseconds when the server accepted it.
    pub created_at_ms: u64,
}

/// Everything needed to create a thread.
///
/// A struct rather than a long argument list so callers name what they mean,
/// and so a new optional field does not change every call site.
#[derive(Clone, Debug)]
pub struct NewThread {
    /// The owning project.
    pub project_id: ProjectId,
    /// A display title; empty strings are normalised to `None`.
    pub title: Option<String>,
    /// The parent thread when this one was delegated from another.
    pub parent_thread_id: Option<ThreadId>,
    /// The execution context, when one is already known.
    pub environment_id: Option<EnvironmentId>,
}

/// The unit of work: one conversation with a provider.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Thread {
    /// Identity.
    pub id: ThreadId,
    /// The owning project.
    pub project_id: ProjectId,
    /// The execution context, once provisioned.
    pub environment_id: Option<EnvironmentId>,
    /// The thread this one was delegated from, if any.
    pub parent_thread_id: Option<ThreadId>,
    /// A display title. Providers often fill this in after the first turn.
    pub title: Option<String>,
    /// Lifecycle status. Mutated only through [`Thread::transition`].
    pub status: ThreadStatus,
    /// Wall-clock milliseconds when the thread was created.
    pub created_at_ms: u64,
    /// Wall-clock milliseconds of the last accepted mutation.
    pub updated_at_ms: u64,
    /// When the thread was archived, mirroring `status == Archived`.
    pub archived_at_ms: Option<u64>,
    /// The provider run currently advancing this thread, when there is one.
    ///
    /// Set when the control plane dispatches a run and cleared when that run
    /// reaches a terminal event. A client can therefore render "which run am I
    /// watching" without a second lookup, and reconciliation can tell an
    /// in-flight thread from one whose run was reaped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_run_id: Option<RunId>,
}

impl Thread {
    /// Creates an `idle` thread and the event it produces.
    pub fn create(new: NewThread, now_ms: u64) -> (Self, DomainEvent) {
        let thread = Self {
            id: ThreadId::mint(),
            project_id: new.project_id,
            environment_id: new.environment_id,
            parent_thread_id: new.parent_thread_id,
            title: new
                .title
                .map(|title| title.trim().to_owned())
                .filter(|title| !title.is_empty()),
            status: ThreadStatus::Idle,
            created_at_ms: now_ms,
            updated_at_ms: now_ms,
            archived_at_ms: None,
            active_run_id: None,
        };
        let event = DomainEvent::ThreadCreated {
            thread: thread.clone(),
        };
        (thread, event)
    }

    /// Applies a lifecycle trigger and returns the resulting status-change
    /// event.
    ///
    /// The status is left untouched when the transition is illegal, so a
    /// rejected trigger cannot partially mutate the thread.
    pub fn transition(
        &mut self,
        trigger: ThreadTrigger,
        now_ms: u64,
    ) -> Result<DomainEvent, DomainError> {
        let from = self.status;
        let Some(to) = from.transition(trigger) else {
            return Err(DomainError::IllegalThreadTransition { from, trigger });
        };
        self.status = to;
        self.updated_at_ms = now_ms;
        if to == ThreadStatus::Archived {
            self.archived_at_ms = Some(now_ms);
        } else if from == ThreadStatus::Archived {
            self.archived_at_ms = None;
        }
        Ok(DomainEvent::ThreadStatusChanged {
            thread_id: self.id.clone(),
            project_id: self.project_id.clone(),
            from,
            to,
            at_ms: now_ms,
        })
    }

    /// Records the run now advancing this thread.
    pub fn begin_run(&mut self, run_id: RunId, now_ms: u64) {
        self.active_run_id = Some(run_id);
        self.updated_at_ms = now_ms;
    }

    /// Clears the recorded run once it has ended.
    pub fn clear_run(&mut self, now_ms: u64) {
        self.active_run_id = None;
        self.updated_at_ms = now_ms;
    }

    /// Appends a message and returns the events the append produces.
    ///
    /// A user message is what drives the thread forward: from `idle` it starts
    /// a run, from `error` it retries, from `waiting` it supplies the awaited
    /// input. So this usually returns two events — `thread_message_added`
    /// followed by `thread_status_changed` — and one event when the status
    /// already allows the message (an assistant message during `working`, a
    /// message into a `waiting` thread from the assistant, and so on).
    ///
    /// Archived threads are read-only.
    pub fn post_message(
        &mut self,
        role: MessageRole,
        content: impl Into<String>,
        now_ms: u64,
    ) -> Result<Vec<DomainEvent>, DomainError> {
        if self.status.is_archived() {
            return Err(DomainError::Archived { entity: "thread" });
        }
        let content = content.into();
        if content.trim().is_empty() {
            return Err(DomainError::InvalidField {
                field: "content",
                reason: "must not be empty".into(),
            });
        }

        let message = ThreadMessage {
            id: MessageId::mint(),
            thread_id: self.id.clone(),
            role,
            content,
            created_at_ms: now_ms,
        };
        let mut events = vec![DomainEvent::ThreadMessageAdded {
            thread_id: self.id.clone(),
            message,
        }];

        let trigger = match (role, self.status) {
            (MessageRole::User, ThreadStatus::Idle) => Some(ThreadTrigger::RunStarted),
            (MessageRole::User, ThreadStatus::Error) => Some(ThreadTrigger::Retry),
            (MessageRole::User, ThreadStatus::Waiting) => Some(ThreadTrigger::InputReceived),
            _ => None,
        };
        if let Some(trigger) = trigger {
            events.push(self.transition(trigger, now_ms)?);
        } else {
            self.updated_at_ms = now_ms;
        }
        Ok(events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn thread() -> Thread {
        let (thread, _) = Thread::create(
            NewThread {
                project_id: ProjectId::mint(),
                title: Some("  first  ".into()),
                parent_thread_id: None,
                environment_id: None,
            },
            1_000,
        );
        thread
    }

    fn assert_illegal(thread: &mut Thread, trigger: ThreadTrigger) {
        let before = thread.clone();
        assert!(matches!(
            thread.transition(trigger, 2_000),
            Err(DomainError::IllegalThreadTransition { .. })
        ));
        assert_eq!(*thread, before, "a rejected trigger must not mutate");
    }

    #[test]
    fn create_starts_idle_and_trims_the_title() {
        let thread = thread();
        assert_eq!(thread.status, ThreadStatus::Idle);
        assert_eq!(thread.title.as_deref(), Some("first"));
        assert_eq!(thread.archived_at_ms, None);
    }

    #[test]
    fn the_happy_path_cycles_through_every_status() {
        let mut thread = thread();

        thread.transition(ThreadTrigger::RunStarted, 10).unwrap();
        assert_eq!(thread.status, ThreadStatus::Working);

        thread.transition(ThreadTrigger::AwaitInput, 20).unwrap();
        assert_eq!(thread.status, ThreadStatus::Waiting);

        thread.transition(ThreadTrigger::InputReceived, 30).unwrap();
        assert_eq!(thread.status, ThreadStatus::Working);

        thread.transition(ThreadTrigger::RunFailed, 40).unwrap();
        assert_eq!(thread.status, ThreadStatus::Error);

        thread.transition(ThreadTrigger::Retry, 50).unwrap();
        assert_eq!(thread.status, ThreadStatus::Working);

        thread.transition(ThreadTrigger::RunCompleted, 60).unwrap();
        assert_eq!(thread.status, ThreadStatus::Idle);

        thread.transition(ThreadTrigger::Archive, 70).unwrap();
        assert_eq!(thread.status, ThreadStatus::Archived);
        assert_eq!(thread.archived_at_ms, Some(70));

        thread.transition(ThreadTrigger::Unarchive, 80).unwrap();
        assert_eq!(thread.status, ThreadStatus::Idle);
        assert_eq!(thread.archived_at_ms, None);
    }

    #[test]
    fn a_waiting_run_can_be_cancelled_back_to_idle() {
        let mut thread = thread();
        thread.transition(ThreadTrigger::RunStarted, 10).unwrap();
        thread.transition(ThreadTrigger::AwaitInput, 20).unwrap();
        thread.transition(ThreadTrigger::RunCancelled, 30).unwrap();
        assert_eq!(thread.status, ThreadStatus::Idle);
    }

    #[test]
    fn every_illegal_transition_is_rejected_without_mutating() {
        // For each status, every trigger that has no cell in the table fails.
        for status in ThreadStatus::ALL {
            for trigger in [
                ThreadTrigger::RunStarted,
                ThreadTrigger::RunCompleted,
                ThreadTrigger::RunFailed,
                ThreadTrigger::AwaitInput,
                ThreadTrigger::InputReceived,
                ThreadTrigger::RunCancelled,
                ThreadTrigger::Retry,
                ThreadTrigger::Archive,
                ThreadTrigger::Unarchive,
            ] {
                let legal = status.transition(trigger).is_some();
                let mut thread = thread();
                thread.status = status;
                if legal {
                    assert!(
                        thread.transition(trigger, 5).is_ok(),
                        "{status} + {trigger} should be legal"
                    );
                } else {
                    assert_illegal(&mut thread, trigger);
                }
            }
        }
    }

    #[test]
    fn archiving_is_legal_from_every_live_status() {
        for status in [
            ThreadStatus::Idle,
            ThreadStatus::Working,
            ThreadStatus::Waiting,
            ThreadStatus::Error,
        ] {
            let mut thread = thread();
            thread.status = status;
            thread.transition(ThreadTrigger::Archive, 9).unwrap();
            assert_eq!(thread.status, ThreadStatus::Archived);
        }
    }

    #[test]
    fn a_user_message_starts_a_run() {
        let mut thread = thread();
        let events = thread
            .post_message(MessageRole::User, "hello", 100)
            .unwrap();

        assert_eq!(thread.status, ThreadStatus::Working);
        assert_eq!(events.len(), 2);
        assert!(matches!(events[0], DomainEvent::ThreadMessageAdded { .. }));
        match &events[1] {
            DomainEvent::ThreadStatusChanged { from, to, .. } => {
                assert_eq!(*from, ThreadStatus::Idle);
                assert_eq!(*to, ThreadStatus::Working);
            }
            other => panic!("expected a status change, got {other:?}"),
        }
    }

    #[test]
    fn a_user_message_retries_an_errored_thread() {
        let mut thread = thread();
        thread.status = ThreadStatus::Error;
        let events = thread
            .post_message(MessageRole::User, "again", 100)
            .unwrap();
        assert_eq!(thread.status, ThreadStatus::Working);
        assert_eq!(events.len(), 2);
    }

    #[test]
    fn an_assistant_message_does_not_move_the_status() {
        let mut thread = thread();
        thread.status = ThreadStatus::Working;
        let events = thread
            .post_message(MessageRole::Assistant, "working on it", 100)
            .unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(thread.status, ThreadStatus::Working);
    }

    #[test]
    fn archived_threads_reject_messages() {
        let mut thread = thread();
        thread.status = ThreadStatus::Archived;
        assert_eq!(
            thread.post_message(MessageRole::User, "hi", 100),
            Err(DomainError::Archived { entity: "thread" })
        );
    }

    #[test]
    fn empty_messages_are_rejected() {
        let mut thread = thread();
        assert!(matches!(
            thread.post_message(MessageRole::User, "   ", 100),
            Err(DomainError::InvalidField {
                field: "content",
                ..
            })
        ));
        assert_eq!(thread.status, ThreadStatus::Idle);
    }
}
