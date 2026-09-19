//! Versioned WebSocket protocols.
//!
//! The public `/ws` surface and the worker `/internal/ws` surface are separate
//! protocols. The relay stores the worker event envelope so replay remains
//! byte-identical; public connections project those domain events into bb's
//! `changed` invalidation messages at the last possible boundary.

use bytes::Bytes;
use loom_domain::{DomainEvent, EnvironmentId, Host, RunId};
use loom_provider_protocol::{
    EnvironmentProvisionReport, HostRpcReport, ProviderCatalogReport, ProviderReport,
};
use loom_relay::envelope::Envelope;
use loom_relay::event_id::EventId;
use loom_relay::scope::Scope;
use serde::{Deserialize, Serialize};

// -----------------------------------------------------------------------------
// Public bb client protocol (`/ws`)
// -----------------------------------------------------------------------------

/// Typed subscription targets accepted by the browser/SDK protocol.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum SubscriptionTarget {
    #[serde(rename = "thread-detail")]
    ThreadDetail {
        #[serde(rename = "threadId")]
        thread_id: String,
    },
    #[serde(rename = "thread-list")]
    ThreadList,
    #[serde(rename = "project-detail")]
    ProjectDetail {
        #[serde(rename = "projectId")]
        project_id: String,
    },
    #[serde(rename = "project-list")]
    ProjectList,
    #[serde(rename = "environment-detail")]
    EnvironmentDetail {
        #[serde(rename = "environmentId")]
        environment_id: String,
    },
    #[serde(rename = "environment-list")]
    EnvironmentList,
    #[serde(rename = "host-detail")]
    HostDetail {
        #[serde(rename = "hostId")]
        host_id: String,
    },
    #[serde(rename = "host-list")]
    HostList,
    #[serde(rename = "system")]
    System,
}

impl SubscriptionTarget {
    /// Contract targets require a non-empty id on detail targets.
    pub fn is_valid(&self) -> bool {
        match self {
            Self::ThreadDetail { thread_id } => !thread_id.is_empty(),
            Self::ProjectDetail { project_id } => !project_id.is_empty(),
            Self::EnvironmentDetail { environment_id } => !environment_id.is_empty(),
            Self::HostDetail { host_id } => !host_id.is_empty(),
            Self::ThreadList
            | Self::ProjectList
            | Self::EnvironmentList
            | Self::HostList
            | Self::System => true,
        }
    }
}

/// Messages a public client may send. There are deliberately no worker
/// commands in this union.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ClientMessage {
    Subscribe { target: SubscriptionTarget },
    Unsubscribe { target: SubscriptionTarget },
    Ping,
}

impl ClientMessage {
    /// Whether all values satisfy the contract's non-empty target constraints.
    pub fn is_valid(&self) -> bool {
        match self {
            Self::Subscribe { target } | Self::Unsubscribe { target } => target.is_valid(),
            Self::Ping => true,
        }
    }
}

/// Entity discriminator in a public `changed` message.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PublicEntity {
    Thread,
    Project,
    Environment,
    Host,
    System,
}

/// All change kinds emitted by the public adapter.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum PublicChangeKind {
    #[serde(rename = "thread-created")]
    ThreadCreated,
    #[serde(rename = "thread-deleted")]
    ThreadDeleted,
    #[serde(rename = "events-appended")]
    EventsAppended,
    #[serde(rename = "history-rewritten")]
    HistoryRewritten,
    #[serde(rename = "interactions-changed")]
    InteractionsChanged,
    #[serde(rename = "status-changed")]
    StatusChanged,
    #[serde(rename = "title-changed")]
    TitleChanged,
    #[serde(rename = "queue-changed")]
    QueueChanged,
    #[serde(rename = "archived-changed")]
    ArchivedChanged,
    #[serde(rename = "pin-state-changed")]
    PinStateChanged,
    #[serde(rename = "parent-changed")]
    ParentChanged,
    #[serde(rename = "environment-changed")]
    EnvironmentChanged,
    #[serde(rename = "read-state-changed")]
    ReadStateChanged,
    #[serde(rename = "order-changed")]
    OrderChanged,
    #[serde(rename = "tabs-changed")]
    TabsChanged,
    #[serde(rename = "terminals-changed")]
    TerminalsChanged,
    #[serde(rename = "project-created")]
    ProjectCreated,
    #[serde(rename = "project-updated")]
    ProjectUpdated,
    #[serde(rename = "project-deleted")]
    ProjectDeleted,
    #[serde(rename = "project-sources-changed")]
    ProjectSourcesChanged,
    #[serde(rename = "threads-changed")]
    ThreadsChanged,
    #[serde(rename = "project-order-changed")]
    ProjectOrderChanged,
    #[serde(rename = "environment-created")]
    EnvironmentCreated,
    #[serde(rename = "environment-deleted")]
    EnvironmentDeleted,
    #[serde(rename = "metadata-changed")]
    MetadataChanged,
    #[serde(rename = "work-status-changed")]
    WorkStatusChanged,
    #[serde(rename = "git-refs-changed")]
    GitRefsChanged,
    #[serde(rename = "thread-storage-changed")]
    ThreadStorageChanged,
    #[serde(rename = "host-connected")]
    HostConnected,
    #[serde(rename = "host-disconnected")]
    HostDisconnected,
    #[serde(rename = "config-changed")]
    ConfigChanged,
    #[serde(rename = "plugins-changed")]
    PluginsChanged,
    #[serde(rename = "provider-registrations-changed")]
    ProviderRegistrationsChanged,
    #[serde(rename = "ui-preferences-changed")]
    UiPreferencesChanged,
    #[serde(rename = "environment-availability-changed")]
    EnvironmentAvailabilityChanged,
}

impl PublicChangeKind {
    fn belongs_to(self, entity: PublicEntity) -> bool {
        match entity {
            PublicEntity::Thread => matches!(
                self,
                Self::ThreadCreated
                    | Self::ThreadDeleted
                    | Self::EventsAppended
                    | Self::HistoryRewritten
                    | Self::InteractionsChanged
                    | Self::StatusChanged
                    | Self::TitleChanged
                    | Self::QueueChanged
                    | Self::ArchivedChanged
                    | Self::PinStateChanged
                    | Self::ParentChanged
                    | Self::EnvironmentChanged
                    | Self::ReadStateChanged
                    | Self::OrderChanged
                    | Self::TabsChanged
                    | Self::TerminalsChanged
            ),
            PublicEntity::Project => matches!(
                self,
                Self::ProjectCreated
                    | Self::ProjectUpdated
                    | Self::ProjectDeleted
                    | Self::ProjectSourcesChanged
                    | Self::ThreadsChanged
                    | Self::ProjectOrderChanged
            ),
            PublicEntity::Environment => matches!(
                self,
                Self::EnvironmentCreated
                    | Self::EnvironmentDeleted
                    | Self::MetadataChanged
                    | Self::StatusChanged
                    | Self::WorkStatusChanged
                    | Self::GitRefsChanged
                    | Self::ThreadStorageChanged
            ),
            PublicEntity::Host => {
                matches!(self, Self::HostConnected | Self::HostDisconnected)
            }
            PublicEntity::System => matches!(
                self,
                Self::ConfigChanged
                    | Self::PluginsChanged
                    | Self::ProviderRegistrationsChanged
                    | Self::UiPreferencesChanged
                    | Self::EnvironmentAvailabilityChanged
            ),
        }
    }
}

/// Runtime display state carried by optional thread status metadata.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PublicDisplayStatus {
    Pending,
    Idle,
    Starting,
    Active,
    Stopping,
    Error,
    Provisioning,
    HostReconnecting,
    WaitingForHost,
}

impl PublicDisplayStatus {
    fn is_durable_thread_status(self) -> bool {
        matches!(
            self,
            Self::Pending
                | Self::Idle
                | Self::Starting
                | Self::Active
                | Self::Stopping
                | Self::Error
        )
    }
}

/// Thread activity counters in bb's optional status metadata.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ThreadActivityMetadata {
    pub active_workflow_count: u64,
    pub active_background_agent_count: u64,
    pub active_background_command_count: u64,
    pub active_plan_mode_count: u64,
    pub active_goal_count: u64,
}

/// Optional runtime status details for a thread change.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ThreadRuntimeMetadata {
    pub display_status: PublicDisplayStatus,
    pub host_reconnect_grace_expires_at: Option<u64>,
}

/// Optional status transition details for a thread change.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ThreadStatusMetadata {
    pub status: PublicDisplayStatus,
    pub runtime: ThreadRuntimeMetadata,
    pub activity: ThreadActivityMetadata,
    pub latest_attention_at: u64,
    pub updated_at: u64,
}

/// Optional metadata accepted by the public contract for thread changes.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ThreadChangeMetadata {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub background_activity_changed: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event_types: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub has_pending_interaction: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_change: Option<ThreadStatusMetadata>,
}

/// Messages a public client may receive. Subscribe acknowledgements, relay
/// envelopes, worker acks and errors are intentionally absent.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ServerMessage {
    Changed {
        entity: PublicEntity,
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        metadata: Option<ThreadChangeMetadata>,
        changes: Vec<PublicChangeKind>,
    },
    Pong,
}

impl ServerMessage {
    /// Rejects combinations the entity-discriminated bb schema cannot express.
    ///
    /// This guard is required for contract-external relay publishing: serde can
    /// decode a shared Rust enum even when a change kind belongs to a different
    /// entity branch in bb's JSON Schema.
    pub fn is_valid(&self) -> bool {
        match self {
            Self::Pong => true,
            Self::Changed {
                entity,
                id,
                metadata,
                changes,
            } => {
                if changes.iter().any(|change| !change.belongs_to(*entity)) {
                    return false;
                }
                match entity {
                    PublicEntity::Thread => metadata.as_ref().is_none_or(|metadata| {
                        metadata.event_types.as_ref().is_none_or(|event_types| {
                            event_types
                                .iter()
                                .all(|kind| loom_domain::ThreadEventType::parse(kind).is_ok())
                        }) && metadata
                            .status_change
                            .as_ref()
                            .is_none_or(|status| status.status.is_durable_thread_status())
                    }),
                    PublicEntity::System => id.is_none() && metadata.is_none(),
                    PublicEntity::Project | PublicEntity::Environment | PublicEntity::Host => {
                        metadata.is_none()
                    }
                }
            }
        }
    }

    /// Whether this change belongs to one typed public subscription target.
    pub fn matches_target(&self, target: &SubscriptionTarget) -> bool {
        let Self::Changed {
            entity,
            id,
            metadata,
            ..
        } = self
        else {
            return false;
        };
        match (target, entity) {
            (SubscriptionTarget::ThreadList, PublicEntity::Thread)
            | (SubscriptionTarget::ProjectList, PublicEntity::Project)
            | (SubscriptionTarget::EnvironmentList, PublicEntity::Environment)
            | (SubscriptionTarget::HostList, PublicEntity::Host)
            | (SubscriptionTarget::System, PublicEntity::System) => true,
            (SubscriptionTarget::ThreadDetail { thread_id }, PublicEntity::Thread)
            | (
                SubscriptionTarget::ProjectDetail {
                    project_id: thread_id,
                },
                PublicEntity::Project,
            )
            | (
                SubscriptionTarget::EnvironmentDetail {
                    environment_id: thread_id,
                },
                PublicEntity::Environment,
            )
            | (SubscriptionTarget::HostDetail { host_id: thread_id }, PublicEntity::Host) => {
                id.as_deref() == Some(thread_id)
            }
            (SubscriptionTarget::ProjectDetail { project_id }, PublicEntity::Thread) => {
                metadata
                    .as_ref()
                    .and_then(|metadata| metadata.project_id.as_deref())
                    == Some(project_id)
            }
            _ => false,
        }
    }
}

// -----------------------------------------------------------------------------
// Internal worker protocol (`/internal/ws`)
// -----------------------------------------------------------------------------

/// Messages a loom worker may send on the internal endpoint.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkerClientMessage {
    Subscribe {
        scope: Scope,
    },
    Unsubscribe {
        scope: Scope,
    },
    Ping,
    EnrollHost {
        #[serde(default)]
        host_id: Option<loom_domain::HostId>,
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        data_dir: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        join_code: Option<String>,
    },
    HostHeartbeat {
        host_id: loom_domain::HostId,
    },
    HostDisconnect {
        host_id: loom_domain::HostId,
    },
    RunReport {
        report: Box<ProviderReport>,
    },
    /// The models the agent on this host advertises, per model ladders included.
    ///
    /// Host-scoped rather than run-scoped: the catalogue describes the agent
    /// installed on the machine, so it is reported once per enrollment and
    /// refreshed from any session the worker opens.
    CatalogReport {
        report: ProviderCatalogReport,
    },
    /// The agents this host found installed, as its own probes verified them.
    ///
    /// Host-scoped and sent after enrollment, as the probes that decide it
    /// settle. A candidate counts only once it has answered an ACP handshake, so
    /// this is evidence about the machine rather than configuration for it. Each
    /// frame is the complete list verified so far, including an empty one — a
    /// host that found nothing replaces whatever it reported before.
    HostProviders {
        host_id: loom_domain::HostId,
        providers: Vec<loom_provider_protocol::ProviderSpec>,
    },
    InteractionRequest {
        request: Box<loom_provider_protocol::InteractionRequest>,
    },
    EnvironmentReport {
        report: EnvironmentProvisionReport,
    },
    HostFileReport {
        report: loom_provider_protocol::HostFileReport,
    },
    HostRpcReport {
        report: HostRpcReport,
    },
    TerminalReport {
        report: loom_provider_protocol::TerminalReport,
    },
    ScriptReport {
        report: loom_provider_protocol::ScriptRunReport,
    },
    Replay {
        scope: Scope,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        since: Option<EventId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<usize>,
    },
}

/// Messages the server sends to an internal worker connection.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkerServerMessage {
    /// First frame on `/internal/ws`; no worker may enroll before checking it.
    Hello {
        protocol_version: u32,
        /// The agents this server can dispatch, so a worker can read each
        /// one's catalogue before any run has happened.
        ///
        /// Optional on the wire: a worker built before the field existed
        /// ignores it, and one built afterwards falls back to its own
        /// configured provider when it is absent. That keeps the frame
        /// compatible in both directions without a protocol bump, and an empty
        /// list is omitted so a single-provider server sends what it always
        /// did.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        providers: Vec<loom_provider_protocol::ProviderSpec>,
    },
    Subscribed {
        scope: Scope,
        first_subscriber: bool,
    },
    Unsubscribed {
        scope: Scope,
    },
    Event {
        event_id: String,
        scope: Scope,
        payload: String,
        created_at_ms: u64,
    },
    Pong,
    Error {
        message: String,
    },
    HostEnrolled {
        host: Host,
        event_id: String,
    },
    HostHeartbeatAck {
        host_id: loom_domain::HostId,
        last_seen_at_ms: u64,
    },
    HostDisconnected {
        host_id: loom_domain::HostId,
    },
    RunReportAck {
        run_id: RunId,
        accepted: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    InteractionRequestAck {
        request_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        interaction_id: Option<String>,
        accepted: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    EnvironmentReportAck {
        environment_id: EnvironmentId,
        accepted: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    ReplayComplete {
        scope: Scope,
        count: usize,
        has_more: bool,
    },
}

// -----------------------------------------------------------------------------
// Relay storage and public projection
// -----------------------------------------------------------------------------

/// Builds the internal frame stored in the relay log.
pub fn build_event_frame(
    scope: &Scope,
    payload: &[u8],
    event_id: EventId,
    created_at_ms: u64,
) -> Bytes {
    let frame = WorkerServerMessage::Event {
        event_id: event_id.to_string(),
        scope: scope.clone(),
        payload: String::from_utf8_lossy(payload).into_owned(),
        created_at_ms,
    };
    Bytes::from(serde_json::to_vec(&frame).expect("a worker frame always serializes to JSON"))
}

/// Converts a stored envelope into the worker frame it contains.
pub fn frame_from_envelope(envelope: &Envelope) -> Bytes {
    envelope.payload.clone()
}

/// Projects one domain event into bb's cache invalidation vocabulary.
///
/// `ThreadUpdated` intentionally emits every field-level invalidation that can
/// be represented by its full-snapshot payload. The domain event predates this
/// adapter and carries the post-update thread, not a mutation diff; broad
/// invalidation is therefore the only replay-safe choice for clears (unpin,
/// unarchive, unread and tab removal included). Deletion is special-cased so a
/// tombstone always reaches bb as `thread-deleted`.
pub fn project_domain_event(event: &DomainEvent) -> Vec<ServerMessage> {
    let primary = match event {
        DomainEvent::ProjectCreated { project } => Some(changed(
            PublicEntity::Project,
            Some(project.id.to_string()),
            vec![PublicChangeKind::ProjectCreated],
        )),
        DomainEvent::ProjectUpdated { project } => {
            let mut changes = vec![
                PublicChangeKind::ProjectUpdated,
                PublicChangeKind::ProjectSourcesChanged,
                PublicChangeKind::ProjectOrderChanged,
            ];
            if project.deleted_at_ms.is_some() {
                changes.push(PublicChangeKind::ProjectDeleted);
            }
            Some(changed(
                PublicEntity::Project,
                Some(project.id.to_string()),
                changes,
            ))
        }
        DomainEvent::ThreadCreated { thread } => Some(thread_changed(
            thread.id.to_string(),
            Some(thread.project_id.to_string()),
            vec![PublicChangeKind::ThreadCreated],
            None,
            None,
        )),
        DomainEvent::ThreadStatusChanged {
            thread_id,
            project_id,
            from,
            to,
            ..
        } => {
            let mut changes = vec![PublicChangeKind::StatusChanged];
            if matches!(from, loom_domain::ThreadStatus::Archived)
                || matches!(to, loom_domain::ThreadStatus::Archived)
            {
                changes.push(PublicChangeKind::ArchivedChanged);
            }
            Some(thread_changed(
                thread_id.to_string(),
                Some(project_id.to_string()),
                changes,
                None,
                None,
            ))
        }
        DomainEvent::ThreadMessageAdded { thread_id, message } => Some(thread_changed(
            thread_id.to_string(),
            message.project_id.as_ref().map(ToString::to_string),
            vec![PublicChangeKind::EventsAppended],
            None,
            None,
        )),
        DomainEvent::ThreadUpdated { thread } => {
            let mut changes = Vec::with_capacity(9);
            if thread.deleted_at_ms.is_some() {
                changes.push(PublicChangeKind::ThreadDeleted);
            }
            changes.extend([
                PublicChangeKind::TitleChanged,
                PublicChangeKind::ArchivedChanged,
                PublicChangeKind::PinStateChanged,
                PublicChangeKind::ParentChanged,
                PublicChangeKind::EnvironmentChanged,
                PublicChangeKind::ReadStateChanged,
                PublicChangeKind::OrderChanged,
                PublicChangeKind::TabsChanged,
            ]);
            Some(thread_changed(
                thread.id.to_string(),
                Some(thread.project_id.to_string()),
                changes,
                None,
                None,
            ))
        }
        DomainEvent::ThreadRunEvent { run } => Some(thread_changed(
            run.thread_id.to_string(),
            Some(run.project_id.to_string()),
            vec![PublicChangeKind::EventsAppended],
            Some(vec![run.kind().to_owned()]),
            None,
        )),
        DomainEvent::ThreadQueuedMessageChanged { queued_message } => Some(thread_changed(
            queued_message.thread_id.to_string(),
            queued_message.project_id.as_ref().map(ToString::to_string),
            vec![PublicChangeKind::QueueChanged],
            None,
            None,
        )),
        DomainEvent::ThreadInteractionChanged { interaction } => Some(thread_changed(
            interaction.thread_id.to_string(),
            interaction.project_id.as_ref().map(ToString::to_string),
            vec![PublicChangeKind::InteractionsChanged],
            None,
            Some(interaction.status.is_open()),
        )),
        DomainEvent::HostRegistered { host } | DomainEvent::HostUpdated { host } => Some(changed(
            PublicEntity::Host,
            Some(host.id.to_string()),
            vec![
                if matches!(host.status, loom_domain::HostStatus::Connected) {
                    PublicChangeKind::HostConnected
                } else {
                    PublicChangeKind::HostDisconnected
                },
            ],
        )),
        DomainEvent::HostStatusChanged { host_id, to, .. } => Some(changed(
            PublicEntity::Host,
            Some(host_id.to_string()),
            vec![if matches!(to, loom_domain::HostStatus::Connected) {
                PublicChangeKind::HostConnected
            } else {
                PublicChangeKind::HostDisconnected
            }],
        )),
        DomainEvent::HostDeleted { host_id, .. } => Some(changed(
            PublicEntity::Host,
            Some(host_id.to_string()),
            vec![PublicChangeKind::HostDisconnected],
        )),
        DomainEvent::EnvironmentCreated { environment } => Some(changed(
            PublicEntity::Environment,
            Some(environment.id.to_string()),
            vec![PublicChangeKind::EnvironmentCreated],
        )),
        DomainEvent::EnvironmentUpdated { environment } => Some(changed(
            PublicEntity::Environment,
            Some(environment.id.to_string()),
            vec![PublicChangeKind::MetadataChanged],
        )),
        DomainEvent::EnvironmentStatusChanged {
            environment_id, to, ..
        } => Some(changed(
            PublicEntity::Environment,
            Some(environment_id.to_string()),
            if matches!(to, loom_domain::EnvironmentStatus::Destroyed) {
                vec![PublicChangeKind::EnvironmentDeleted]
            } else {
                vec![
                    PublicChangeKind::StatusChanged,
                    PublicChangeKind::WorkStatusChanged,
                ]
            },
        )),
        // Sections are workspace-wide sidebar state. bb has no section entity,
        // so the thread-list owner is invalidated without pretending a thread
        // id was changed.
        DomainEvent::ThreadSectionCreated { .. }
        | DomainEvent::ThreadSectionUpdated { .. }
        | DomainEvent::ThreadSectionDeleted { .. } => Some(changed(
            PublicEntity::Thread,
            None,
            vec![PublicChangeKind::OrderChanged],
        )),
    };

    let mut messages: Vec<_> = primary.into_iter().collect();
    if matches!(
        event,
        DomainEvent::HostRegistered { .. }
            | DomainEvent::HostStatusChanged { .. }
            | DomainEvent::HostUpdated { .. }
            | DomainEvent::HostDeleted { .. }
            | DomainEvent::EnvironmentCreated { .. }
            | DomainEvent::EnvironmentUpdated { .. }
            | DomainEvent::EnvironmentStatusChanged { .. }
    ) {
        messages.push(changed(
            PublicEntity::System,
            None,
            vec![PublicChangeKind::EnvironmentAvailabilityChanged],
        ));
    }
    messages
}

fn thread_changed(
    id: String,
    project_id: Option<String>,
    changes: Vec<PublicChangeKind>,
    event_types: Option<Vec<String>>,
    has_pending_interaction: Option<bool>,
) -> ServerMessage {
    let metadata = ThreadChangeMetadata {
        project_id,
        event_types,
        has_pending_interaction,
        ..ThreadChangeMetadata::default()
    };
    let metadata = if metadata == ThreadChangeMetadata::default() {
        None
    } else {
        Some(metadata)
    };
    ServerMessage::Changed {
        entity: PublicEntity::Thread,
        id: Some(id),
        metadata,
        changes,
    }
}

fn changed(
    entity: PublicEntity,
    id: Option<String>,
    changes: Vec<PublicChangeKind>,
) -> ServerMessage {
    ServerMessage::Changed {
        entity,
        id,
        metadata: None,
        changes,
    }
}

/// Turns an internal relay frame into public messages, dropping raw worker
/// traffic and payloads with no bb representation.
pub fn public_messages_from_frame(frame: &[u8]) -> Vec<ServerMessage> {
    if let Ok(WorkerServerMessage::Event { payload, .. }) =
        serde_json::from_slice::<WorkerServerMessage>(frame)
    {
        if let Ok(event) = serde_json::from_str::<DomainEvent>(&payload) {
            return project_domain_event(&event);
        }
        return serde_json::from_str::<ServerMessage>(&payload)
            .ok()
            .filter(ServerMessage::is_valid)
            .into_iter()
            .collect();
    }
    // Retained for direct control-frame tests; production pongs are sent by
    // the public socket task and are not written to the relay.
    serde_json::from_slice::<ServerMessage>(frame)
        .ok()
        .filter(ServerMessage::is_valid)
        .into_iter()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_protocol_uses_typed_targets_and_only_allowed_messages() {
        let subscribe: ClientMessage = serde_json::from_str(
            r#"{"type":"subscribe","target":{"kind":"thread-detail","threadId":"thr_1"}}"#,
        )
        .unwrap();
        assert_eq!(
            subscribe,
            ClientMessage::Subscribe {
                target: SubscriptionTarget::ThreadDetail {
                    thread_id: "thr_1".into()
                }
            }
        );
        assert!(
            serde_json::from_str::<ClientMessage>(r#"{"type":"enroll_host","name":"laptop"}"#)
                .is_err()
        );
        let empty: ClientMessage = serde_json::from_str(
            r#"{"type":"subscribe","target":{"kind":"thread-detail","threadId":""}}"#,
        )
        .unwrap();
        assert!(!empty.is_valid());
    }

    #[test]
    fn public_messages_match_the_exported_wire_shape() {
        let message = ServerMessage::Changed {
            entity: PublicEntity::Thread,
            id: Some("thr_1".into()),
            metadata: None,
            changes: vec![PublicChangeKind::ThreadDeleted],
        };
        let value = serde_json::to_value(message).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "type": "changed",
                "entity": "thread",
                "id": "thr_1",
                "changes": ["thread-deleted"]
            })
        );
        let contract = loom_contract::Contract::load();
        assert!(
            contract
                .validate_server_message("client", &value)
                .is_empty(),
            "public message diverged from exported bb schema"
        );
        let pong = serde_json::to_value(ServerMessage::Pong).unwrap();
        assert!(contract.validate_server_message("client", &pong).is_empty());
    }

    #[test]
    fn an_automation_invalidation_reaches_project_subscribers() {
        // What the automations side publishes: bb's public vocabulary has no
        // automation entity, so the frame is the project's own change — and it
        // has to be a shape both the contract and the matching table accept.
        let message = ServerMessage::Changed {
            entity: PublicEntity::Project,
            id: Some("proj_1".into()),
            metadata: None,
            changes: vec![PublicChangeKind::ProjectUpdated],
        };
        assert!(message.is_valid());

        // The project the view is looking at, and the workspace-wide list: both
        // are targets the pinned client asks for.
        assert!(message.matches_target(&SubscriptionTarget::ProjectDetail {
            project_id: "proj_1".into()
        }));
        assert!(message.matches_target(&SubscriptionTarget::ProjectList));
        // A different project is not told about this one's automations, and a
        // thread subscription is not a project subscription.
        assert!(!message.matches_target(&SubscriptionTarget::ProjectDetail {
            project_id: "proj_2".into()
        }));
        assert!(!message.matches_target(&SubscriptionTarget::ThreadList));

        let value = serde_json::to_value(&message).unwrap();
        assert!(
            loom_contract::Contract::load()
                .validate_server_message("client", &value)
                .is_empty(),
            "the invalidation diverged from the exported bb schema: {value}"
        );
    }

    #[test]
    fn public_projection_rejects_cross_entity_and_invalid_metadata_payloads() {
        let invalid = [
            serde_json::json!({
                "type": "changed",
                "entity": "system",
                "changes": ["thread-deleted"]
            }),
            serde_json::json!({
                "type": "changed",
                "entity": "project",
                "id": "project",
                "metadata": { "projectId": "project" },
                "changes": ["project-updated"]
            }),
            serde_json::json!({
                "type": "changed",
                "entity": "thread",
                "id": "thread",
                "metadata": { "eventTypes": ["not/a/thread-event"] },
                "changes": ["events-appended"]
            }),
            serde_json::json!({
                "type": "changed",
                "entity": "thread",
                "id": "thread",
                "metadata": {
                    "statusChange": {
                        "status": "provisioning",
                        "runtime": {
                            "displayStatus": "provisioning",
                            "hostReconnectGraceExpiresAt": null
                        },
                        "activity": {
                            "activeWorkflowCount": 0,
                            "activeBackgroundAgentCount": 0,
                            "activeBackgroundCommandCount": 0,
                            "activePlanModeCount": 0,
                            "activeGoalCount": 0
                        },
                        "latestAttentionAt": 0,
                        "updatedAt": 0
                    }
                },
                "changes": ["status-changed"]
            }),
        ];

        for payload in invalid {
            let frame = build_event_frame(
                &Scope::Global,
                &serde_json::to_vec(&payload).unwrap(),
                EventId::new(),
                1,
            );
            assert!(public_messages_from_frame(&frame).is_empty(), "{payload}");
        }
    }

    #[test]
    fn worker_handshake_is_distinct_from_the_public_protocol() {
        let hello = WorkerServerMessage::Hello {
            protocol_version: 3,
            providers: Vec::new(),
        };
        assert_eq!(
            serde_json::to_value(hello).unwrap(),
            serde_json::json!({ "type": "hello", "protocol_version": 3 })
        );
        assert!(
            serde_json::from_str::<ClientMessage>(r#"{"type":"hello","protocol_version":3}"#)
                .is_err()
        );
    }

    /// The provider list is additive on the wire: a hello with agents carries
    /// them, and one without them still parses — which is what lets a worker
    /// built before the field existed keep enrolling.
    #[test]
    fn a_hello_may_carry_the_provider_list() {
        let providers = vec![loom_provider_protocol::ProviderSpec::pi()];
        let hello = WorkerServerMessage::Hello {
            protocol_version: 3,
            providers: providers.clone(),
        };
        let encoded = serde_json::to_value(&hello).unwrap();
        assert_eq!(
            encoded["providers"],
            serde_json::to_value(&providers).unwrap()
        );

        let without =
            serde_json::from_str::<WorkerServerMessage>(r#"{"type":"hello","protocol_version":3}"#)
                .unwrap();
        assert_eq!(
            without,
            WorkerServerMessage::Hello {
                protocol_version: 3,
                providers: Vec::new(),
            }
        );
    }

    #[test]
    fn deleted_thread_events_project_to_thread_deleted() {
        let (mut thread, _) = loom_domain::Thread::create(
            loom_domain::NewThread {
                project_id: loom_domain::ProjectId::mint(),
                title: None,
                parent_thread_id: None,
                environment_id: None,
            },
            1,
        );
        let event = thread.mark_deleted(2).unwrap();
        let message = project_domain_event(&event).into_iter().next().unwrap();
        let ServerMessage::Changed { changes, .. } = message else {
            panic!("expected a changed message");
        };
        assert!(changes.contains(&PublicChangeKind::ThreadDeleted));
    }

    #[test]
    fn run_events_carry_project_and_provider_event_metadata() {
        let thread_id = loom_domain::ThreadId::mint();
        let project_id = loom_domain::ProjectId::mint();
        let run = loom_domain::RunEvent::failed(
            thread_id.clone(),
            project_id.clone(),
            loom_domain::RunId::mint(),
            1,
            loom_domain::TurnStatus::Failed,
            "preflight failed",
        );
        let messages = project_domain_event(&DomainEvent::ThreadRunEvent { run: Box::new(run) });
        let message = messages.first().unwrap();
        assert!(message.matches_target(&SubscriptionTarget::ProjectDetail {
            project_id: project_id.to_string(),
        }));
        let value = serde_json::to_value(message).unwrap();
        assert_eq!(value["metadata"]["projectId"], project_id.to_string());
        assert_eq!(
            value["metadata"]["eventTypes"],
            serde_json::json!(["turn/completed"])
        );
        assert!(loom_contract::Contract::load()
            .validate_server_message("client", &value)
            .is_empty());
    }

    #[test]
    fn internal_event_frames_project_to_public_changes() {
        let thread_id = loom_domain::ThreadId::mint();
        let scope = Scope::Thread(thread_id.to_string());
        let event_id = EventId::new();
        let event = loom_domain::DomainEvent::ThreadStatusChanged {
            thread_id: thread_id.clone(),
            project_id: loom_domain::ProjectId::mint(),
            from: loom_domain::ThreadStatus::Idle,
            to: loom_domain::ThreadStatus::Working,
            at_ms: 1,
        };
        let frame = build_event_frame(&scope, &serde_json::to_vec(&event).unwrap(), event_id, 1);
        let public = public_messages_from_frame(&frame)
            .into_iter()
            .next()
            .unwrap();
        assert!(public.matches_target(&SubscriptionTarget::ThreadDetail {
            thread_id: thread_id.to_string()
        }));
    }
}
