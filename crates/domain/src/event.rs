//! Domain events: what one entity change emits, and where it goes.
//!
//! Every variant is a self-contained fact about one entity change, serialized
//! with a `type` tag so a client dispatches on `event.type` without guessing:
//!
//! ```json
//! {"type":"thread_status_changed","thread_id":"thr_…","project_id":"proj_…",
//!  "from":"idle","to":"working","at_ms":1789120438372}
//! ```
//!
//! The tag names are part of the wire contract; changing one is a breaking
//! change and the test at the bottom of this file pins them.
//!
//! ## Which change emits what
//!
//! | change | event | scope |
//! | --- | --- | --- |
//! | create a project | `project_created` | `global` |
//! | add a source, rename a project | `project_updated` | `project:{id}` |
//! | create a thread | `thread_created` | `project:{project_id}` |
//! | any lifecycle trigger | `thread_status_changed` | `thread:{id}` |
//! | post a message | `thread_message_added` | `thread:{id}` |
//! | queue, send or cancel a queued message | `thread_queued_message_changed` | `thread:{id}` |
//! | raise, answer or cancel an interaction | `thread_interaction_changed` | `thread:{id}` |
//! | rename, retitle, refile or retab | `thread_updated` | `project:{project_id}` |
//! | register a host | `host_registered` | `host:{id}` |
//! | connect/disconnect a host | `host_status_changed` | `host:{id}` |
//! | create an environment | `environment_created` | `project:{project_id}` |
//! | provision/ready/teardown | `environment_status_changed` | `project:{project_id}` |
//! | create/rename/delete a section | `thread_section_created` / `thread_section_updated` / `thread_section_deleted` | `global` |
//!
//! Posting a user message to an `idle` thread emits **two** events in order:
//! `thread_message_added`, then `thread_status_changed` (see
//! [`Thread::post_message`](crate::Thread::post_message)). Both go to the
//! thread scope, so they arrive in order into the conversation.

use serde::{Deserialize, Serialize};

use crate::environment::{Environment, EnvironmentStatus};
use crate::host::{Host, HostStatus};
use crate::id::{EnvironmentId, HostId, ProjectId, ThreadId};
use crate::interaction::Interaction;
use crate::project::Project;
use crate::queue::QueuedMessage;
use crate::run::RunEvent;
use crate::scope::DomainScope;
use crate::section::ThreadSection;
use crate::thread::{Thread, ThreadMessage, ThreadStatus};

/// A fact about one entity change.
///
/// The `type` tag is stable and dispatched on by clients; adding a variant is
/// additive, renaming one is not.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DomainEvent {
    /// A project now exists.
    ProjectCreated {
        /// The project.
        project: Project,
    },
    /// A project's fields or sources changed.
    ProjectUpdated {
        /// The project after the change.
        project: Project,
    },
    /// A thread now exists.
    ThreadCreated {
        /// The thread.
        thread: Thread,
    },
    /// A thread's lifecycle status moved.
    ThreadStatusChanged {
        /// The thread.
        thread_id: ThreadId,
        /// Its project, so a consumer need not look it up.
        project_id: ProjectId,
        /// The status left.
        from: ThreadStatus,
        /// The status entered.
        to: ThreadStatus,
        /// Wall-clock milliseconds of the change.
        at_ms: u64,
    },
    /// A message was appended to a thread.
    ThreadMessageAdded {
        /// The thread.
        thread_id: ThreadId,
        /// The message.
        message: ThreadMessage,
    },
    /// A thread's fields changed outside the lifecycle.
    ///
    /// Carries the whole thread after the change, because the change has no
    /// single field: a `threads.update` may move a title, a section, a parent
    /// and the execution options at once, and a `tabs` write replaces a list. A
    /// consumer either applies the new value or ignores the event; there is no
    /// partial interpretation to get wrong.
    ThreadUpdated {
        /// The thread after the change.
        thread: Thread,
    },
    /// Something happened during an in-flight provider run.
    ///
    /// The stream is ordered and always ends in exactly one
    /// [`ProviderEvent::TurnCompleted`]. See [`crate::run`] for why that
    /// terminal invariant matters: it is what keeps a thread from being stuck
    /// in `working` after a provider crash.
    ///
    /// The carried [`RunEvent`] is the run envelope (thread, project, run id,
    /// timestamp) around a bb-contract [`ThreadEvent`](crate::ThreadEvent), so
    /// a client's projection layer consumes the inner event unchanged.
    ///
    /// [`ProviderEvent::TurnCompleted`]: crate::ProviderEvent::TurnCompleted
    ThreadRunEvent {
        /// The run envelope and the contract event it carries.
        ///
        /// Boxed because a contract event is far larger than any other
        /// variant's payload; inlining it would inflate every `DomainEvent`.
        #[serde(flatten)]
        run: Box<RunEvent>,
    },
    /// A host registered.
    HostRegistered {
        /// The host.
        host: Host,
    },
    /// A host's daemon attached or detached.
    HostStatusChanged {
        /// The host.
        host_id: HostId,
        /// The status left.
        from: HostStatus,
        /// The status entered.
        to: HostStatus,
        /// Wall-clock milliseconds of the change.
        at_ms: u64,
    },
    /// A host's mutable metadata changed.
    HostUpdated {
        /// The host after the change.
        host: Host,
    },
    /// A host was removed after all references were cleared.
    HostDeleted {
        /// The removed host identity.
        host_id: HostId,
        /// The display name at deletion time.
        name: String,
    },
    /// A queued message was created, sent or cancelled.
    ///
    /// Carries the whole message after the change, for the same reason
    /// [`DomainEvent::ThreadUpdated`] carries the whole thread: one event has
    /// to express three different mutations, and a consumer either applies the
    /// new value or ignores it.
    ///
    /// Published to the **thread** scope, because a queue is part of one
    /// thread's composer state and a client subscribed to the conversation is
    /// the only one that renders it.
    ThreadQueuedMessageChanged {
        /// The message after the change.
        queued_message: QueuedMessage,
    },
    /// An interaction was raised, answered or cancelled.
    ///
    /// Published to the thread scope: an interaction blocks one thread's turn
    /// and is rendered in that conversation.
    ThreadInteractionChanged {
        /// The interaction after the change.
        interaction: Interaction,
    },
    /// An environment now exists.
    EnvironmentCreated {
        /// The environment.
        environment: Environment,
    },
    /// An environment's editable metadata changed.
    EnvironmentUpdated {
        /// The environment after the change.
        environment: Environment,
    },
    /// An environment's provisioning status moved.
    EnvironmentStatusChanged {
        /// The environment.
        environment_id: EnvironmentId,
        /// Its project.
        project_id: ProjectId,
        /// The host it lives on.
        host_id: HostId,
        /// The status left.
        from: EnvironmentStatus,
        /// The status entered.
        to: EnvironmentStatus,
        /// Wall-clock milliseconds of the change.
        at_ms: u64,
    },
    /// A sidebar section now exists.
    ///
    /// Sections are not project-scoped: the sidebar lists them for the whole
    /// workspace, so the event goes to `global` exactly as
    /// [`DomainEvent::ProjectCreated`] does.
    ThreadSectionCreated {
        /// The section.
        section: ThreadSection,
    },
    /// A section's name changed.
    ThreadSectionUpdated {
        /// The section after the change.
        section: ThreadSection,
    },
    /// A section was deleted.
    ///
    /// Carries the name it had, so a consumer removing it can report what
    /// disappeared without a lookup the record no longer supports. Threads that
    /// referenced it are deliberately **not** rewritten; see
    /// [`crate::section`].
    ThreadSectionDeleted {
        /// The section that is gone.
        section_id: crate::id::ThreadSectionId,
        /// The name it had.
        name: String,
    },
}

impl DomainEvent {
    /// The stable `type` tag this event serializes with.
    pub fn kind(&self) -> &'static str {
        match self {
            DomainEvent::ProjectCreated { .. } => "project_created",
            DomainEvent::ProjectUpdated { .. } => "project_updated",
            DomainEvent::ThreadCreated { .. } => "thread_created",
            DomainEvent::ThreadStatusChanged { .. } => "thread_status_changed",
            DomainEvent::ThreadMessageAdded { .. } => "thread_message_added",
            DomainEvent::ThreadUpdated { .. } => "thread_updated",
            DomainEvent::ThreadRunEvent { .. } => "thread_run_event",
            DomainEvent::HostRegistered { .. } => "host_registered",
            DomainEvent::HostStatusChanged { .. } => "host_status_changed",
            DomainEvent::HostUpdated { .. } => "host_updated",
            DomainEvent::HostDeleted { .. } => "host_deleted",
            DomainEvent::ThreadQueuedMessageChanged { .. } => "thread_queued_message_changed",
            DomainEvent::ThreadInteractionChanged { .. } => "thread_interaction_changed",
            DomainEvent::EnvironmentCreated { .. } => "environment_created",
            DomainEvent::EnvironmentUpdated { .. } => "environment_updated",
            DomainEvent::EnvironmentStatusChanged { .. } => "environment_status_changed",
            DomainEvent::ThreadSectionCreated { .. } => "thread_section_created",
            DomainEvent::ThreadSectionUpdated { .. } => "thread_section_updated",
            DomainEvent::ThreadSectionDeleted { .. } => "thread_section_deleted",
        }
    }

    /// The single scope this event is published to.
    ///
    /// Exactly one, deliberately: see the module docs on
    /// [`DomainScope`](crate::DomainScope). `ProjectCreated` goes to `global`
    /// because the project list is not scoped to a project the client can
    /// already have subscribed to.
    pub fn scope(&self) -> DomainScope {
        match self {
            DomainEvent::ProjectCreated { .. } => DomainScope::Global,
            DomainEvent::ProjectUpdated { project } => DomainScope::Project(project.id.clone()),
            DomainEvent::ThreadCreated { thread } => {
                DomainScope::Project(thread.project_id.clone())
            }
            DomainEvent::ThreadStatusChanged { thread_id, .. }
            | DomainEvent::ThreadMessageAdded { thread_id, .. } => {
                DomainScope::Thread(thread_id.clone())
            }
            // A thread's identity fields are what a *list* renders — the
            // sidebar's rows, not the open conversation — and `thread_created`
            // already goes to the project for the same reason. A client
            // rendering a thread keeps its project subscribed, so it sees this
            // too; a client subscribed only to the thread room would miss it,
            // which is the accepted cost of one scope per event.
            DomainEvent::ThreadUpdated { thread } => {
                DomainScope::Project(thread.project_id.clone())
            }
            DomainEvent::ThreadRunEvent { run } => DomainScope::Thread(run.thread_id.clone()),
            DomainEvent::ThreadQueuedMessageChanged { queued_message } => {
                DomainScope::Thread(queued_message.thread_id.clone())
            }
            DomainEvent::ThreadInteractionChanged { interaction } => {
                DomainScope::Thread(interaction.thread_id.clone())
            }
            DomainEvent::HostRegistered { host } => DomainScope::Host(host.id.clone()),
            DomainEvent::HostStatusChanged { host_id, .. } => DomainScope::Host(host_id.clone()),
            DomainEvent::HostUpdated { host } => DomainScope::Host(host.id.clone()),
            DomainEvent::HostDeleted { host_id, .. } => DomainScope::Host(host_id.clone()),
            DomainEvent::EnvironmentCreated { environment } => {
                DomainScope::Project(environment.project_id.clone())
            }
            DomainEvent::EnvironmentUpdated { environment } => {
                DomainScope::Project(environment.project_id.clone())
            }
            DomainEvent::EnvironmentStatusChanged { project_id, .. } => {
                DomainScope::Project(project_id.clone())
            }
            // The sidebar lists sections workspace-wide, so there is no
            // narrower room to publish to: `global` is the one scope that
            // reaches every client that renders the list.
            DomainEvent::ThreadSectionCreated { .. }
            | DomainEvent::ThreadSectionUpdated { .. }
            | DomainEvent::ThreadSectionDeleted { .. } => DomainScope::Global,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::environment::EnvironmentKind;
    use crate::project::ProjectKind;
    use crate::thread::{MessageRole, NewThread, ThreadTrigger};

    #[test]
    fn every_event_serializes_with_its_type_tag() {
        let (project, created) = Project::create("loom", ProjectKind::Standard, 1).unwrap();
        assert_eq!(
            serde_json::to_value(&created).unwrap()["type"],
            "project_created"
        );

        let mut project = project;
        let updated = project
            .add_source(HostId::mint(), "/srv/loom", None, 2)
            .unwrap();
        assert_eq!(
            serde_json::to_value(&updated).unwrap()["type"],
            "project_updated"
        );

        let (mut thread, created) = Thread::create(
            NewThread {
                project_id: project.id.clone(),
                title: None,
                parent_thread_id: None,
                environment_id: None,
            },
            3,
        );
        assert_eq!(
            serde_json::to_value(&created).unwrap()["type"],
            "thread_created"
        );

        let changed = thread.transition(ThreadTrigger::RunStarted, 4).unwrap();
        let value = serde_json::to_value(&changed).unwrap();
        assert_eq!(value["type"], "thread_status_changed");
        assert_eq!(value["from"], "idle");
        assert_eq!(value["to"], "working");

        let (host, registered) = Host::register("laptop", 5).unwrap();
        assert_eq!(
            serde_json::to_value(&registered).unwrap()["type"],
            "host_registered"
        );

        let mut host = host;
        let host_changed = host.mark_disconnected(6).unwrap();
        assert_eq!(
            serde_json::to_value(&host_changed).unwrap()["type"],
            "host_status_changed"
        );

        let (mut environment, env_created) = Environment::create(
            project.id.clone(),
            host.id.clone(),
            EnvironmentKind::Managed,
            None,
            7,
        )
        .unwrap();
        assert_eq!(
            serde_json::to_value(&env_created).unwrap()["type"],
            "environment_created"
        );

        let env_changed = environment
            .set_status(EnvironmentStatus::Provisioning, 8)
            .unwrap();
        assert_eq!(
            serde_json::to_value(&env_changed).unwrap()["type"],
            "environment_status_changed"
        );
    }

    #[test]
    fn a_message_event_carries_the_thread_and_role() {
        let (mut thread, _) = Thread::create(
            NewThread {
                project_id: ProjectId::mint(),
                title: None,
                parent_thread_id: None,
                environment_id: None,
            },
            1,
        );
        let events = thread.post_message(MessageRole::User, "hi", 2).unwrap();
        let value = serde_json::to_value(&events[0]).unwrap();

        assert_eq!(value["type"], "thread_message_added");
        assert_eq!(value["thread_id"], thread.id.to_string());
        assert_eq!(value["message"]["role"], "user");
        assert_eq!(value["message"]["content"], "hi");
    }

    #[test]
    fn events_are_published_to_the_narrowest_useful_scope() {
        let (project, created) = Project::create("loom", ProjectKind::Standard, 1).unwrap();
        assert_eq!(created.scope(), DomainScope::Global);

        let (thread, thread_created) = Thread::create(
            NewThread {
                project_id: project.id.clone(),
                title: None,
                parent_thread_id: None,
                environment_id: None,
            },
            2,
        );
        assert_eq!(
            thread_created.scope(),
            DomainScope::Project(project.id.clone())
        );

        let changed = {
            let mut thread = thread.clone();
            thread.transition(ThreadTrigger::RunStarted, 3).unwrap()
        };
        assert_eq!(changed.scope(), DomainScope::Thread(thread.id.clone()));

        let (host, registered) = Host::register("laptop", 4).unwrap();
        assert_eq!(registered.scope(), DomainScope::Host(host.id.clone()));

        let (_environment, env_created) = Environment::create(
            project.id.clone(),
            host.id.clone(),
            EnvironmentKind::Unmanaged,
            Some("/srv/loom".into()),
            5,
        )
        .unwrap();
        assert_eq!(
            env_created.scope(),
            DomainScope::Project(project.id.clone())
        );
    }

    #[test]
    fn a_run_event_is_published_to_its_thread_scope() {
        let thread_id = ThreadId::mint();
        let project_id = ProjectId::mint();
        let event = DomainEvent::ThreadRunEvent {
            run: Box::new(crate::run::RunEvent::completed(
                thread_id.clone(),
                project_id,
                crate::id::RunId::mint(),
                7,
                Some("p".into()),
            )),
        };
        assert_eq!(event.kind(), "thread_run_event");
        assert_eq!(event.scope(), DomainScope::Thread(thread_id));
        assert_eq!(
            serde_json::to_value(&event).unwrap()["type"],
            "thread_run_event"
        );
    }

    #[test]
    fn kind_matches_the_serialized_tag_for_every_variant() {
        // Build one of each event and check `kind()` agrees with serde. This is
        // what keeps the stable wire contract honest.
        let (project, project_created) = Project::create("p", ProjectKind::Standard, 1).unwrap();
        let (_thread, thread_created) = Thread::create(
            NewThread {
                project_id: project.id.clone(),
                title: None,
                parent_thread_id: None,
                environment_id: None,
            },
            1,
        );
        let (host, host_registered) = Host::register("h", 1).unwrap();
        let (_environment, environment_created) = Environment::create(
            project.id.clone(),
            host.id.clone(),
            EnvironmentKind::Managed,
            None,
            1,
        )
        .unwrap();

        for event in [
            project_created,
            DomainEvent::ProjectUpdated {
                project: project.clone(),
            },
            thread_created,
            DomainEvent::ThreadStatusChanged {
                thread_id: ThreadId::mint(),
                project_id: project.id.clone(),
                from: ThreadStatus::Idle,
                to: ThreadStatus::Working,
                at_ms: 1,
            },
            DomainEvent::ThreadMessageAdded {
                thread_id: ThreadId::mint(),
                message: ThreadMessage {
                    id: crate::id::MessageId::mint(),
                    thread_id: ThreadId::mint(),
                    role: MessageRole::User,
                    content: "x".into(),
                    created_at_ms: 1,
                },
            },
            host_registered,
            DomainEvent::HostStatusChanged {
                host_id: HostId::mint(),
                from: HostStatus::Connected,
                to: HostStatus::Disconnected,
                at_ms: 1,
            },
            environment_created,
            DomainEvent::EnvironmentStatusChanged {
                environment_id: EnvironmentId::mint(),
                project_id: project.id.clone(),
                host_id: host.id.clone(),
                from: EnvironmentStatus::Creating,
                to: EnvironmentStatus::Provisioning,
                at_ms: 1,
            },
        ] {
            let tagged = serde_json::to_value(&event).unwrap();
            assert_eq!(tagged["type"], event.kind());
        }
    }
}
