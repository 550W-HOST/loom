//! The loom domain model.
//!
//! Pure types and invariants for the four core entities — **Project**,
//! **Thread**, **Host** and **Environment** — plus the events a change to any
//! of them produces and the scope each event belongs to.
//!
//! Three properties are the reason this is a crate of its own:
//!
//! 1. **No IO, no async, no storage.** Nothing here opens a file, awaits a
//!    future or depends on `loom-relay`. Any layer — server, daemon, a future
//!    CLI or migration tool — can depend on it without dragging a runtime
//!    along, and every invariant is testable with plain unit tests.
//! 2. **Typed identities.** A [`ThreadId`] is not a `String` and not a
//!    [`HostId`]; see [`id`]. Prefixes are validated on parse and on
//!    deserialize, so a mis-typed id cannot cross a wire boundary either.
//! 3. **An explicit lifecycle.** A thread's status only changes through
//!    [`ThreadStatus::transition`], so an illegal transition is a typed error
//!    rather than a state that should not exist.
//!
//! # Scope mapping
//!
//! The relay routes by `(kind, id)`. The domain names those pairs with
//! [`DomainScope`], and [`DomainEvent::scope`] assigns each event exactly one:
//!
//! | scope | what lives there |
//! | --- | --- |
//! | `global` | the project list; events with no narrower room (`project_created`) |
//! | `project:{id}` | a project's list-level state: `project_updated`, `thread_created`, `thread_updated`, `environment_*` |
//! | `thread:{id}` | one conversation: `thread_status_changed`, `thread_message_added` |
//! | `host:{id}` | one machine's daemon room: `host_registered`, `host_status_changed` |
//! | `user:{id}` | one user's clients. Reserved; no user entity exists yet |
//!
//! One event, one scope: a client subscribes to the two scopes it is
//! displaying, and a fact delivered under two event ids would be a duplicate it
//! cannot deduplicate. The full change-to-event table is in [`event`].
//!
//! # Thread lifecycle
//!
//! ```text
//!   idle ──run_started──▶ working ──run_completed──▶ idle
//!                          │  ▲
//!            await_input   │  │ input_received
//!                          ▼  │
//!                        waiting ──run_cancelled──▶ idle
//!                          │
//!   working ──run_failed──▶ error ──retry──▶ working
//!   waiting ──run_failed──▶ error
//!
//!   any non-archived status ──archive──▶ archived ──unarchive──▶ idle
//! ```
//!
//! See [`thread`] for the meaning of each status and the authoritative
//! transition table.

#![forbid(unsafe_code)]

pub mod environment;
pub mod error;
pub mod event;
pub mod host;
pub mod id;
pub mod project;
pub mod provider_event;
pub mod run;
pub mod scope;
pub mod thread;

pub use environment::{Environment, EnvironmentKind, EnvironmentStatus};
pub use error::DomainError;
pub use event::DomainEvent;
pub use host::{select_primary_host, Host, HostKind, HostStatus};
pub use id::{
    is_turn_request_id, mint_turn_request_id, Entity, EnvironmentId, EnvironmentTag, HostId,
    HostTag, Id, MessageId, MessageTag, ProjectId, ProjectSourceId, ProjectSourceTag, ProjectTag,
    RunId, RunTag, ThreadId, ThreadTag, UserId, UserTag,
};
pub use project::{Project, ProjectKind, ProjectSource};
pub use provider_event::{
    ApprovalStatus, ContextWindowUsage, EnvResolvedEntry, EnvResolvedSource, EnvResolvedValue,
    FileChange, FileChangeKind, GoalStatus, ItemPresentation, ItemStatus, ModelFallbackReason,
    PlanStep, PlanStepStatus, PresentationBadge, PresentationIcon, PresentationLabel,
    PresentationTint, PresentationTone, ProviderErrorCategory, ProviderErrorInfo, ProviderEvent,
    ProviderEventError, ProviderEventType, ProviderWarningCategory, SearchMode, ThreadEvent,
    ThreadEventItem, ThreadEventScope, ThreadEventType, ThreadTokenUsage, TokenUsageBreakdown,
    TurnError, TurnStatus, UserContent,
};
pub use run::{RunEvent, RunOutcome};
pub use scope::DomainScope;
pub use thread::{
    MessageRole, NewThread, ReasoningLevel, Thread, ThreadMessage, ThreadStatus, ThreadTrigger,
    ThreadUpdate, ThreadVisibility,
};
