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
use crate::id::{EnvironmentId, HostId, MessageId, ProjectId, RunId, ThreadId};

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

/// Whether a thread is shown in the default sidebar (`threads.update`).
///
/// Deliberately independent of [`ThreadStatus::Archived`]: archiving is a
/// lifecycle step a client reacts to, hiding is a display preference the
/// client applies to the list it renders.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadVisibility {
    /// Shown in the default sidebar.
    #[default]
    Visible,
    /// Kept out of the default sidebar.
    Hidden,
}

impl ThreadVisibility {
    /// The spelling bb's `threadVisibilitySchema` uses.
    pub fn as_str(self) -> &'static str {
        match self {
            ThreadVisibility::Visible => "visible",
            ThreadVisibility::Hidden => "hidden",
        }
    }
}

impl fmt::Display for ThreadVisibility {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why a thread was created outside the normal root/child flow.
///
/// Fork is the only origin currently represented by the public contract. The
/// enum is intentionally closed so an unsupported origin cannot leak into a
/// thread projection that clients validate strictly.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThreadOriginKind {
    /// The thread was created from another thread's history.
    Fork,
}

impl ThreadOriginKind {
    /// The stable wire token.
    pub fn as_str(self) -> &'static str {
        match self {
            ThreadOriginKind::Fork => "fork",
        }
    }
}

impl fmt::Display for ThreadOriginKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How much reasoning the provider is asked to spend on a turn.
///
/// Opaque to loom: the value is one the *provider* advertised for the model the
/// session holds, carried unchanged from the client's choice back to the agent.
/// Which levels exist is therefore the agent's business, and it follows the
/// selected model — pi derives the ladder per model from that model's own
/// thinking-level map. bb's closed `reasoningLevelSchema` is a display
/// vocabulary the client does not enforce: it renders the values the server
/// advertises and sends one back verbatim, so loom stores and forwards those
/// rather than consulting a list of its own.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ReasoningLevel(String);

impl ReasoningLevel {
    /// The provider's own id for the level.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The id as the provider spells it.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for ReasoningLevel {
    fn from(id: &str) -> Self {
        Self(id.to_owned())
    }
}

impl From<String> for ReasoningLevel {
    fn from(id: String) -> Self {
        Self(id)
    }
}

impl fmt::Display for ReasoningLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The fields `threads.update` may change on a stored thread.
///
/// Every optional field is a double option, because the contract distinguishes
/// the two kinds of absence: an omitted field keeps its value, an explicit
/// `null` clears it (`"title": "string | null"`). `visibility` has no null
/// branch, so it is a plain option — omitted means unchanged.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadUpdate {
    /// A new display title, or `Some(None)` to clear it.
    #[serde(default, deserialize_with = "double_option")]
    pub title: Option<Option<String>>,
    /// The thread this one is delegated from, or `Some(None)` to detach it.
    #[serde(default, deserialize_with = "double_option")]
    pub parent_thread_id: Option<Option<ThreadId>>,
    /// The sidebar section this thread belongs to, or `Some(None)` for the
    /// default section.
    #[serde(default, deserialize_with = "double_option")]
    pub section_id: Option<Option<String>>,
    /// The model this thread's next run starts with, or `Some(None)` to fall
    /// back to the server's configured model.
    #[serde(default, deserialize_with = "double_option")]
    pub model: Option<Option<String>>,
    /// The reasoning level this thread's next run asks for, or `Some(None)`
    /// for the server's default.
    #[serde(default, deserialize_with = "double_option")]
    pub reasoning_level: Option<Option<ReasoningLevel>>,
    /// The provider this thread's next run uses, or `Some(None)` for the
    /// server's default.
    #[serde(default, deserialize_with = "double_option")]
    pub provider_id: Option<Option<String>>,
    /// Whether the thread is shown in the default sidebar.
    #[serde(default)]
    pub visibility: Option<ThreadVisibility>,
}

/// Deserializes `T` as `Some(T)` even when the JSON value is `null`, so a
/// field of type `Option<Option<T>>` can tell "omitted" from "null".
fn double_option<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
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
    /// Owning project, retained so cross-node realtime projection needs no lookup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<ProjectId>,
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

/// What a thread's provider session id is bound to.
///
/// A provider session id is the *agent's* identifier, unique within that agent
/// and meaningful only for the directory the session was opened in. So resuming
/// one requires both: resuming with a different agent would hand it an id it
/// never issued, and resuming in a different directory would reopen a
/// conversation about the wrong workspace.
///
/// It is also the *machine's* identifier: the id names a file on one host's
/// disk. A host that never issued it cannot restore it, and two machines can
/// hold sessions that happen to share an id for paths that happen to match, so
/// the owning host is part of the claim rather than a lookup hint.
///
/// Recorded from the same `thread/identity` event that established the id.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderSessionBinding {
    /// The provider that issued the id, as `ProviderSpec::name`.
    pub agent: String,
    /// The workspace the session was opened in.
    pub cwd: String,
    /// The host whose agent issued the id, when known.
    ///
    /// `None` only for a binding recorded before this field existed. It means
    /// "unknown", not "any": a caller that needs the session restored must
    /// treat it as unprovable and ask for a re-bind rather than guess a
    /// machine that happens to share the path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_id: Option<HostId>,
    /// When the binding was recorded, for a log or a support report. Not used
    /// for any decision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bound_at_ms: Option<u64>,
}

impl ProviderSessionBinding {
    /// A binding for `agent` in `cwd`, with no host attributed yet.
    pub fn new(agent: impl Into<String>, cwd: impl Into<String>) -> Self {
        Self {
            agent: agent.into(),
            cwd: cwd.into(),
            host_id: None,
            bound_at_ms: None,
        }
    }

    /// The same binding, attributed to the host that issued the id.
    pub fn on_host(mut self, host_id: HostId) -> Self {
        self.host_id = Some(host_id);
        self
    }

    /// The same binding, stamped with when it was recorded.
    pub fn at(mut self, now_ms: u64) -> Self {
        self.bound_at_ms = Some(now_ms);
        self
    }
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
    /// When the thread was soft-deleted.
    ///
    /// Deleted threads remain in the registry and snapshots as tombstones so
    /// replay cannot resurrect them when an older creation event is retained.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deleted_at_ms: Option<u64>,
    /// Wall-clock milliseconds when a client last marked the thread read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_read_at_ms: Option<u64>,
    /// The provider run currently advancing this thread, when there is one.
    ///
    /// Set when the control plane dispatches a run and cleared when that run
    /// reaches a terminal event. A client can therefore render "which run am I
    /// watching" without a second lookup, and reconciliation can tell an
    /// in-flight thread from one whose run was reaped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_run_id: Option<RunId>,
    /// The sidebar section this thread was filed under, when a client filed it.
    ///
    /// Opaque to the domain: sections are a client-side grouping that loom does
    /// not model yet, so the id is stored and reported rather than resolved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub section_id: Option<String>,
    /// Whether the thread is shown in the default sidebar.
    #[serde(default)]
    pub visibility: ThreadVisibility,
    /// The model a new run of this thread starts with, when a client chose one.
    ///
    /// Recorded and reported (`threads.defaultExecutionOptions`), and carried
    /// into the dispatch as the agent's own model id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The reasoning level a new run of this thread asks for, when chosen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_level: Option<ReasoningLevel>,
    /// The provider this thread runs on, when a client chose one.
    ///
    /// A machine may serve several agents, so a run must go to the one the
    /// user picked rather than to whichever the control plane lists first.
    /// `None` means the server's default provider, which is what every thread
    /// created before this field existed gets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_id: Option<String>,
    /// The thread's open tabs, exactly as the client sent them.
    ///
    /// View state, not domain state: the shape is the client's
    /// (`tabsSchema`), and loom stores it opaquely so a tab kind it does not
    /// know still round-trips through a reload. The contract middleware
    /// validates the shape before it is ever stored.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tabs: Vec<serde_json::Value>,
    /// The revision a client must name to replace `tabs` (compare-and-swap).
    ///
    /// Starts at `0` and increases by one per accepted write, so two clients
    /// editing the same thread's tabs cannot silently overwrite each other.
    #[serde(default)]
    pub tabs_revision: u64,
    /// The agent's own identifier for this thread's conversation.
    ///
    /// loom does not invent this: an ACP agent returns it from `session/new`
    /// and accepts it back through `session/load` (or the versioned resume
    /// method), and it is what makes a second turn continue the first one's
    /// conversation instead of starting over. Reported to the log as
    /// `providerThreadId`, learned back from the `thread/identity` event, and
    /// stored here so a dispatch can replay it.
    ///
    /// Domain state rather than a client-visible field: the contract's thread
    /// shape has no place for it, so it is persisted and used without being
    /// serialized into an HTTP response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_session_id: Option<String>,
    /// The source thread when this thread came from a fork.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_thread_id: Option<ThreadId>,
    /// The non-standard creation origin, when one exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_kind: Option<ThreadOriginKind>,
    /// The plugin that requested the origin, when applicable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_plugin_id: Option<String>,
    /// When the thread was pinned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pinned_at_ms: Option<u64>,
    /// Stable fractional ordering key among pinned threads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pin_sort_key: Option<String>,
    /// The agent that id belongs to, and the workspace it was opened in.
    ///
    /// A provider session id is **not globally unique**: it is the agent's own
    /// identifier, unique only within that agent, and a session is bound to the
    /// directory it was created in. So `(agent, session id, cwd)` is the
    /// identity, and resuming with a different agent or a different workspace
    /// would hand one agent's id to another (or reopen a conversation about a
    /// directory that no longer exists).
    ///
    /// Both are recorded from the same `thread/identity` event that
    /// established the id, and a dispatch only carries the id when the agent
    /// and workspace still match. A mismatch starts a fresh session rather than
    /// guessing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_session_binding: Option<ProviderSessionBinding>,
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
            deleted_at_ms: None,
            last_read_at_ms: None,
            active_run_id: None,
            section_id: None,
            visibility: ThreadVisibility::Visible,
            model: None,
            reasoning_level: None,
            provider_id: None,
            tabs: Vec::new(),
            tabs_revision: 0,
            provider_session_id: None,
            source_thread_id: None,
            origin_kind: None,
            origin_plugin_id: None,
            pinned_at_ms: None,
            pin_sort_key: None,
            provider_session_binding: None,
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

    /// Records that a client has consumed the thread up to `now_ms`.
    pub fn mark_read(&mut self, now_ms: u64) {
        self.last_read_at_ms = Some(now_ms);
    }

    /// Changes the read marker and returns a replayable update when it moved.
    pub fn set_read_at(
        &mut self,
        last_read_at_ms: Option<u64>,
        now_ms: u64,
    ) -> Option<DomainEvent> {
        if self.last_read_at_ms == last_read_at_ms {
            return None;
        }
        self.last_read_at_ms = last_read_at_ms;
        self.updated_at_ms = now_ms;
        Some(DomainEvent::ThreadUpdated {
            thread: self.clone(),
        })
    }

    /// Marks this thread deleted without removing its tombstone.
    pub fn mark_deleted(&mut self, now_ms: u64) -> Option<DomainEvent> {
        if self.deleted_at_ms.is_some() {
            return None;
        }
        self.deleted_at_ms = Some(now_ms);
        self.updated_at_ms = now_ms;
        Some(DomainEvent::ThreadUpdated {
            thread: self.clone(),
        })
    }

    /// Changes pin state and returns a replayable update when it moved.
    pub fn set_pin(
        &mut self,
        pinned_at_ms: Option<u64>,
        pin_sort_key: Option<String>,
        now_ms: u64,
    ) -> Option<DomainEvent> {
        if self.pinned_at_ms == pinned_at_ms && self.pin_sort_key == pin_sort_key {
            return None;
        }
        self.pinned_at_ms = pinned_at_ms;
        self.pin_sort_key = pin_sort_key;
        self.updated_at_ms = now_ms;
        Some(DomainEvent::ThreadUpdated {
            thread: self.clone(),
        })
    }

    /// Applies a client's field changes, returning the event when anything
    /// actually changed.
    ///
    /// `None` means the update was a no-op — every field it named already held
    /// the requested value, or it named none at all. That is what keeps an idempotent
    /// `threads.update` from bumping `updated_at_ms` and reordering the sidebar.
    ///
    /// The parent is only checked for self-reference here; whether it exists
    /// and shares this thread's project is the registry's business, because a
    /// thread cannot see its siblings.
    pub fn apply_update(
        &mut self,
        update: &ThreadUpdate,
        now_ms: u64,
    ) -> Result<Option<DomainEvent>, DomainError> {
        let mut changed = false;

        if let Some(title) = &update.title {
            let title = title
                .as_ref()
                .map(|title| title.trim().to_owned())
                .filter(|title| !title.is_empty());
            if title.is_none() && update.title.as_ref().is_some_and(Option::is_some) {
                return Err(DomainError::InvalidField {
                    field: "title",
                    reason: "must not be blank".into(),
                });
            }
            changed |= self.title != title;
            self.title = title;
        }

        if let Some(parent) = &update.parent_thread_id {
            if parent.as_ref() == Some(&self.id) {
                return Err(DomainError::InvalidField {
                    field: "parentThreadId",
                    reason: "a thread cannot be its own parent".into(),
                });
            }
            changed |= &self.parent_thread_id != parent;
            self.parent_thread_id = parent.clone();
        }

        if let Some(section) = &update.section_id {
            let section = section
                .as_ref()
                .map(|section| section.trim().to_owned())
                .filter(|section| !section.is_empty());
            changed |= self.section_id != section;
            self.section_id = section;
        }

        if let Some(model) = &update.model {
            let model = model
                .as_ref()
                .map(|model| model.trim().to_owned())
                .filter(|model| !model.is_empty());
            changed |= self.model != model;
            self.model = model;
        }

        if let Some(level) = &update.reasoning_level {
            changed |= self.reasoning_level != *level;
            self.reasoning_level = level.clone();
        }

        if let Some(provider_id) = &update.provider_id {
            let provider_id = provider_id
                .as_ref()
                .map(|provider_id| provider_id.trim().to_owned())
                .filter(|provider_id| !provider_id.is_empty());
            changed |= self.provider_id != provider_id;
            self.provider_id = provider_id;
        }

        if let Some(visibility) = update.visibility {
            changed |= self.visibility != visibility;
            self.visibility = visibility;
        }

        if !changed {
            return Ok(None);
        }
        self.updated_at_ms = now_ms;
        Ok(Some(DomainEvent::ThreadUpdated {
            thread: self.clone(),
        }))
    }

    /// Replaces the thread's tabs under a compare-and-swap revision.
    ///
    /// `expected_revision` must equal [`Thread::tabs_revision`]; a mismatch is
    /// [`DomainError::TabsConflict`] rather than a lost update, which is the
    /// only reason the revision exists.
    pub fn set_tabs(
        &mut self,
        tabs: Vec<serde_json::Value>,
        expected_revision: u64,
        now_ms: u64,
    ) -> Result<DomainEvent, DomainError> {
        if expected_revision != self.tabs_revision {
            return Err(DomainError::TabsConflict {
                expected: expected_revision,
                current: self.tabs_revision,
            });
        }
        self.tabs = tabs;
        self.tabs_revision = self.tabs_revision.saturating_add(1);
        self.updated_at_ms = now_ms;
        Ok(DomainEvent::ThreadUpdated {
            thread: self.clone(),
        })
    }

    /// Records the agent's identifier for this thread's conversation.
    ///
    /// Returns the event when the value actually changed, and `None` when it
    /// did not: the agent reports its identity on every turn, so a repeat is
    /// the common case and republishing would put a fact in the log that says
    /// nothing new. Once known, the value is never cleared — a session that
    /// exists still exists even if a later turn fails.
    ///
    /// `binding` is the agent and workspace the session belongs to. It is
    /// recorded alongside the id because the id alone cannot answer whether a
    /// later run may resume it: see [`Thread::provider_session_binding`].
    pub fn set_provider_session_id(
        &mut self,
        session_id: impl Into<String>,
        binding: Option<ProviderSessionBinding>,
        now_ms: u64,
    ) -> Option<DomainEvent> {
        let session_id = session_id.into();
        if session_id.is_empty() {
            return None;
        }
        let unchanged = self.provider_session_id.as_deref() == Some(&session_id)
            && self.provider_session_binding == binding;
        if unchanged {
            return None;
        }
        // The binding moves with the id, including when only the binding
        // changed: a session re-reported from a different workspace makes the
        // old binding a stale claim about where that conversation lives, and a
        // stale binding is exactly what lets a dispatch resume in the wrong
        // directory.
        self.provider_session_id = Some(session_id);
        self.provider_session_binding = binding;
        self.updated_at_ms = now_ms;
        Some(DomainEvent::ThreadUpdated {
            thread: self.clone(),
        })
    }

    /// Names an untitled thread from the agent's own title for the
    /// conversation.
    ///
    /// The ACP adapter reports the agent's session title as
    /// `thread/name/updated`, and for an agent like pi it is derived from the
    /// first message. loom generates no titles of its own, so this is the one
    /// automatic source a thread has.
    ///
    /// It is applied only while the thread has no title: a title a client
    /// created the thread with, or set through `threads.update`, outranks the
    /// agent's guess. The agent re-reports its name on later turns of the same
    /// session, so an unconditional write would silently undo a rename.
    ///
    /// Returns the event when the title changed, and `None` when it did not or
    /// the thread already had one.
    pub fn set_provider_title(&mut self, title: &str, now_ms: u64) -> Option<DomainEvent> {
        let title = title.trim();
        if title.is_empty() || self.title.is_some() {
            return None;
        }
        self.title = Some(title.to_owned());
        self.updated_at_ms = now_ms;
        Some(DomainEvent::ThreadUpdated {
            thread: self.clone(),
        })
    }

    /// Whether a run in `cwd` on `host` by `agent` may resume this thread's
    /// session.
    ///
    /// The whole point of recording the binding: a session opened by a
    /// different agent, in a different workspace, or on a different machine
    /// must be started fresh. Resuming across any of those boundaries would
    /// hand an agent an id it never issued, reopen a conversation about a
    /// directory that is not the one being edited, or ask a machine for a
    /// session that lives on another machine's disk — and none of those
    /// failures is visible until well after it has done damage. Two machines
    /// can hold sessions that share an id for paths that happen to match, so
    /// the host is part of the claim rather than a lookup hint.
    ///
    /// A thread whose binding is unknown (`None`, a snapshot written before
    /// W-566) or whose host was never recorded is **not** resumable: the safe
    /// reading of a missing binding is "cannot prove this is the same
    /// conversation", and a fresh session is recoverable while the wrong
    /// resume is not.
    pub fn may_resume_session(&self, agent: &str, cwd: &str, host: &HostId) -> bool {
        if self.provider_session_id.is_none() {
            return false;
        }
        self.provider_session_binding
            .as_ref()
            .is_some_and(|binding| {
                binding.agent == agent
                    && binding.cwd == cwd
                    && binding.host_id.as_ref() == Some(host)
            })
    }

    /// The session id to dispatch for a run in `cwd` on `host` by `agent`, if
    /// any.
    ///
    /// `None` means "start a new session", which is what a mismatched binding
    /// and an absent one both mean. The caller never has to re-derive the
    /// condition. See [`Thread::may_resume_session`].
    pub fn resumable_session_id(&self, agent: &str, cwd: &str, host: &HostId) -> Option<&str> {
        self.may_resume_session(agent, cwd, host)
            .then_some(self.provider_session_id.as_deref())
            .flatten()
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
            project_id: Some(self.project_id.clone()),
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
    use serde_json::json;

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

    #[test]
    fn an_update_that_changes_nothing_produces_no_event() {
        let mut thread = thread();
        let before = thread.clone();
        assert_eq!(
            thread
                .apply_update(&ThreadUpdate::default(), 2_000)
                .unwrap(),
            None
        );
        assert_eq!(thread, before, "an empty update must not touch the thread");

        // Naming the value a field already holds is the same no-op.
        let same = ThreadUpdate {
            title: Some(Some("first".into())),
            visibility: Some(ThreadVisibility::Visible),
            ..ThreadUpdate::default()
        };
        assert_eq!(thread.apply_update(&same, 2_000).unwrap(), None);
        assert_eq!(thread.updated_at_ms, before.updated_at_ms);
    }

    #[test]
    fn an_update_reports_the_thread_it_produced() {
        let mut thread = thread();
        let update: ThreadUpdate = serde_json::from_value(json!({
            "title": "  renamed  ",
            "sectionId": "sec-1",
            "visibility": "hidden",
            "model": "pi",
            "reasoningLevel": "xhigh",
            "providerId": "  codex  ",
        }))
        .unwrap();
        let event = thread.apply_update(&update, 2_000).unwrap().unwrap();
        let DomainEvent::ThreadUpdated { thread: updated } = &event else {
            panic!("expected a thread_updated event, got {event:?}");
        };
        assert_eq!(updated.title.as_deref(), Some("renamed"));
        assert_eq!(updated.section_id.as_deref(), Some("sec-1"));
        assert_eq!(updated.visibility, ThreadVisibility::Hidden);
        assert_eq!(updated.model.as_deref(), Some("pi"));
        assert_eq!(updated.reasoning_level, Some(ReasoningLevel::from("xhigh")));
        assert_eq!(
            updated.provider_id.as_deref(),
            Some("codex"),
            "the provider id is trimmed like a model id"
        );
        assert_eq!(updated.updated_at_ms, 2_000);
        assert_eq!(updated.tabs_revision, 0, "an update must not move the tabs");
    }

    #[test]
    fn an_omitted_field_is_kept_and_an_explicit_null_clears_it() {
        let mut thread = thread();
        thread.section_id = Some("sec-1".into());
        thread.model = Some("pi".into());
        thread.provider_id = Some("codex".into());

        // Omitted: `sectionId` keeps its value while `model` is cleared.
        let update: ThreadUpdate =
            serde_json::from_value(json!({ "model": null, "title": "kept" })).unwrap();
        assert_eq!(update.section_id, None);
        assert_eq!(update.model, Some(None));
        thread.apply_update(&update, 2_000).unwrap();
        assert_eq!(thread.section_id.as_deref(), Some("sec-1"));
        assert_eq!(thread.model, None);
        assert_eq!(
            thread.provider_id.as_deref(),
            Some("codex"),
            "an omitted provider is kept, not cleared alongside the model"
        );
        assert_eq!(thread.title.as_deref(), Some("kept"));

        // Explicit null: `sectionId` is cleared.
        let update: ThreadUpdate = serde_json::from_value(json!({ "sectionId": null })).unwrap();
        thread.apply_update(&update, 2_001).unwrap();
        assert_eq!(thread.section_id, None);

        // And so is the provider.
        let update: ThreadUpdate = serde_json::from_value(json!({ "providerId": null })).unwrap();
        thread.apply_update(&update, 2_002).unwrap();
        assert_eq!(thread.provider_id, None);
    }

    #[test]
    fn a_blank_title_is_rejected_rather_than_silently_cleared() {
        let mut thread = thread();
        let update = ThreadUpdate {
            title: Some(Some("   ".into())),
            ..ThreadUpdate::default()
        };
        assert!(matches!(
            thread.apply_update(&update, 2_000),
            Err(DomainError::InvalidField { field: "title", .. })
        ));
        assert_eq!(thread.title.as_deref(), Some("first"));
    }

    // --- provider title ---------------------------------------------------

    #[test]
    fn a_provider_title_names_an_untitled_thread_only() {
        // A title a client set outranks the agent's name, whether it came from
        // `threads.create` or a later rename.
        let mut titled = thread();
        assert_eq!(titled.set_provider_title("from the agent", 2_000), None);
        assert_eq!(titled.title.as_deref(), Some("first"));

        let mut thread = thread();
        thread
            .apply_update(
                &ThreadUpdate {
                    title: Some(None),
                    ..ThreadUpdate::default()
                },
                1_500,
            )
            .unwrap();
        assert_eq!(thread.title, None);

        let event = thread
            .set_provider_title("  from the agent  ", 2_000)
            .expect("an untitled thread takes the agent's name");
        let DomainEvent::ThreadUpdated { thread: updated } = &event else {
            panic!("expected a thread_updated event, got {event:?}");
        };
        assert_eq!(updated.title.as_deref(), Some("from the agent"));
        assert_eq!(updated.updated_at_ms, 2_000);

        // The agent re-reports its name every turn; only the first is a fact.
        assert_eq!(thread.set_provider_title("from the agent", 3_000), None);

        // And once a client renames the thread, the agent's name cannot undo it.
        thread
            .apply_update(
                &ThreadUpdate {
                    title: Some(Some("renamed".into())),
                    ..ThreadUpdate::default()
                },
                3_000,
            )
            .unwrap();
        assert_eq!(thread.set_provider_title("from the agent", 3_001), None);
        assert_eq!(thread.title.as_deref(), Some("renamed"));

        // A blank name is not a title.
        thread
            .apply_update(
                &ThreadUpdate {
                    title: Some(None),
                    ..ThreadUpdate::default()
                },
                4_000,
            )
            .unwrap();
        assert_eq!(thread.set_provider_title("   ", 4_001), None);
        assert_eq!(thread.title, None);
    }

    #[test]
    fn a_thread_cannot_be_its_own_parent() {
        let mut thread = thread();
        let update = ThreadUpdate {
            parent_thread_id: Some(Some(thread.id.clone())),
            ..ThreadUpdate::default()
        };
        assert!(matches!(
            thread.apply_update(&update, 2_000),
            Err(DomainError::InvalidField {
                field: "parentThreadId",
                ..
            })
        ));
    }

    #[test]
    fn tabs_are_written_under_a_compare_and_swap_revision() {
        let mut thread = thread();
        let tabs = vec![json!({ "id": "tab-1", "kind": "thread-info" })];
        let event = thread.set_tabs(tabs.clone(), 0, 2_000).unwrap();
        let DomainEvent::ThreadUpdated { thread: updated } = &event else {
            panic!("expected a thread_updated event, got {event:?}");
        };
        assert_eq!(updated.tabs_revision, 1);
        assert_eq!(updated.tabs, tabs);

        // A stale revision loses the write and changes nothing.
        let before = thread.clone();
        assert_eq!(
            thread.set_tabs(vec![], 0, 2_001),
            Err(DomainError::TabsConflict {
                expected: 0,
                current: 1
            })
        );
        assert_eq!(thread, before);
    }

    // --- provider session binding -----------------------------------------

    #[test]
    fn a_session_is_resumable_only_by_its_own_agent_workspace_and_host() {
        let mut thread = thread();
        let host = HostId::mint();
        // No session at all: nothing to resume.
        assert!(!thread.may_resume_session("pi", "/srv/a", &host));
        assert_eq!(thread.resumable_session_id("pi", "/srv/a", &host), None);

        thread
            .set_provider_session_id(
                "acp-1",
                Some(ProviderSessionBinding::new("pi", "/srv/a").on_host(host.clone())),
                1,
            )
            .expect("the first id changes the thread");

        assert!(thread.may_resume_session("pi", "/srv/a", &host));
        assert_eq!(
            thread.resumable_session_id("pi", "/srv/a", &host),
            Some("acp-1")
        );
        // A different agent must not be handed this id: it never issued it.
        assert!(!thread.may_resume_session("omp", "/srv/a", &host));
        assert_eq!(thread.resumable_session_id("omp", "/srv/a", &host), None);
        // Neither must a different workspace: the conversation is about /srv/a.
        assert!(!thread.may_resume_session("pi", "/srv/b", &host));
        assert_eq!(thread.resumable_session_id("pi", "/srv/b", &host), None);
        // Nor another machine: the id names a file on *that* host's disk, and
        // two machines can hold sessions that share an id for matching paths.
        let other = HostId::mint();
        assert!(!thread.may_resume_session("pi", "/srv/a", &other));
        assert_eq!(thread.resumable_session_id("pi", "/srv/a", &other), None);
    }

    #[test]
    fn a_session_with_no_binding_is_not_resumed() {
        // The shape an older snapshot deserializes into: an id with no record
        // of where it came from. Guessing is the wrong move — a fresh session
        // is recoverable and a wrong resume is not.
        let mut thread = thread();
        thread.provider_session_id = Some("acp-1".into());
        let host = HostId::mint();
        assert!(!thread.may_resume_session("pi", "/srv/a", &host));
        assert_eq!(thread.resumable_session_id("pi", "/srv/a", &host), None);
    }

    #[test]
    fn a_binding_without_a_host_is_not_resumed() {
        // A binding written before the host was recorded proves the agent and
        // the workspace but not the machine. An unproven host is unproven.
        let mut thread = thread();
        thread
            .set_provider_session_id(
                "acp-1",
                Some(ProviderSessionBinding::new("pi", "/srv/a")),
                1,
            )
            .unwrap();
        assert!(!thread.may_resume_session("pi", "/srv/a", &HostId::mint()));
    }

    #[test]
    fn a_binding_from_before_hosts_were_recorded_still_deserializes() {
        // The snapshot-compat guarantee: an additive field must not make an
        // older snapshot unreadable. `hostId` is absent, so the host is
        // unproven rather than absent-and-therefore-any.
        let binding: ProviderSessionBinding =
            serde_json::from_str(r#"{"agent":"pi","cwd":"/srv/a","bound_at_ms":7}"#).unwrap();
        assert_eq!(binding.agent, "pi");
        assert_eq!(binding.cwd, "/srv/a");
        assert_eq!(binding.host_id, None);
        assert_eq!(binding.bound_at_ms, Some(7));
    }

    #[test]
    fn re_binding_a_session_updates_where_it_may_be_resumed() {
        let mut thread = thread();
        let first = HostId::mint();
        let second = HostId::mint();
        thread
            .set_provider_session_id(
                "acp-1",
                Some(ProviderSessionBinding::new("pi", "/srv/a").on_host(first.clone())),
                1,
            )
            .unwrap();
        // The same id reported from a different workspace: the old binding is a
        // stale claim about where the conversation lives, so it must not win.
        let event = thread
            .set_provider_session_id(
                "acp-1",
                Some(ProviderSessionBinding::new("pi", "/srv/b").on_host(second.clone())),
                2,
            )
            .expect("a changed binding is a change");
        assert!(matches!(event, DomainEvent::ThreadUpdated { .. }));
        assert!(!thread.may_resume_session("pi", "/srv/a", &first));
        assert_eq!(
            thread.resumable_session_id("pi", "/srv/b", &second),
            Some("acp-1")
        );
    }

    #[test]
    fn learning_the_host_of_an_existing_binding_is_a_change() {
        // A legacy binding proves the agent and the workspace. Re-reporting the
        // same session from the machine that owns it must upgrade the claim
        // rather than be dropped as a repeat: until it does, the thread is not
        // resumable.
        let mut thread = thread();
        thread
            .set_provider_session_id(
                "acp-1",
                Some(ProviderSessionBinding::new("pi", "/srv/a")),
                1,
            )
            .unwrap();
        let host = HostId::mint();
        let event = thread
            .set_provider_session_id(
                "acp-1",
                Some(ProviderSessionBinding::new("pi", "/srv/a").on_host(host.clone())),
                2,
            )
            .expect("a binding that gains its host is a change");
        assert!(matches!(event, DomainEvent::ThreadUpdated { .. }));
        assert!(thread.may_resume_session("pi", "/srv/a", &host));
    }

    #[test]
    fn re_reporting_the_same_id_and_binding_publishes_nothing() {
        let mut thread = thread();
        let binding = ProviderSessionBinding::new("pi", "/srv/a");
        assert!(thread
            .set_provider_session_id("acp-1", Some(binding.clone()), 1)
            .is_some());
        // The agent reports its identity every turn; a repeat is the common
        // case and must not put a fact in the log that says nothing new.
        assert_eq!(
            thread.set_provider_session_id("acp-1", Some(binding), 2),
            None
        );
        assert_eq!(thread.updated_at_ms, 1);
    }
}
