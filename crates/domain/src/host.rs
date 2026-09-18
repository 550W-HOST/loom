//! Hosts: the identity a worker registers.
//!
//! A host is a *machine*, not a connection. Its status tracks whether a worker
//! is currently attached; the id survives reconnects. There is deliberately no
//! provider or plugin registration on a host — providers are first-class
//! elsewhere, and hosts only say "this machine can run work".

use serde::{Deserialize, Serialize};

use crate::error::DomainError;
use crate::event::DomainEvent;
use crate::id::HostId;

/// The kind of machine a host identifies.
///
/// Only one kind exists today. It is an enum rather than a bool so a future
/// ephemeral runner is an added variant, not a reinterpreted field.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostKind {
    /// A long-lived machine that workers enroll.
    #[default]
    Persistent,
}

/// The maximum provider permission mode a host may grant.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HostPermissionMode {
    /// The provider may edit files, but not use unrestricted access.
    AcceptEdits,
    /// The provider may use the normal automatic policy.
    Auto,
    /// The provider may use all ACP capabilities.
    #[default]
    Full,
}

impl HostPermissionMode {
    /// The wire spelling used by the bb contract and ACP policy.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AcceptEdits => "accept-edits",
            Self::Auto => "auto",
            Self::Full => "full",
        }
    }

    /// Whether `requested` is within this ceiling.
    pub const fn allows(self, requested: Self) -> bool {
        self.rank() >= requested.rank()
    }

    const fn rank(self) -> u8 {
        match self {
            Self::AcceptEdits => 0,
            Self::Auto => 1,
            Self::Full => 2,
        }
    }
}

/// Whether a worker is currently attached to this host.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostStatus {
    /// A worker holds an active connection.
    Connected,
    /// No worker is attached; the host still exists.
    Disconnected,
}

/// The identity of a machine that runs work.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Host {
    /// Identity, stable across reconnects.
    pub id: HostId,
    /// Display name, chosen by the operator or the machine.
    pub name: String,
    /// Machine kind.
    pub kind: HostKind,
    /// Whether a worker is attached.
    pub status: HostStatus,
    /// Wall-clock milliseconds of the last heartbeat, if ever seen.
    pub last_seen_at_ms: Option<u64>,
    /// Wall-clock milliseconds when the host was first registered.
    pub created_at_ms: u64,
    /// Wall-clock milliseconds of the last mutation.
    pub updated_at_ms: u64,
    /// The maximum ACP permission mode allowed on this host.
    #[serde(default)]
    pub max_permission_mode: HostPermissionMode,
    /// The worker's own data directory on this machine, as it reported at
    /// enrollment.
    ///
    /// Thread storage is a directory the **worker** owns
    /// (`<data_dir>/thread-storage/<thread_id>`), so the control plane can only
    /// name it if the machine told it where its data lives. Recorded at
    /// enrollment and kept across a disconnect on purpose: a
    /// `threads.storageLocation` read is a question about the layout, and it
    /// should not start failing merely because the worker is briefly away.
    ///
    /// `None` means the host never reported one — an older worker, or a host
    /// enrolled through a test or the reference HTTP endpoint. A storage route
    /// then answers `501 not_configured` rather than inventing a path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_dir: Option<String>,
}

impl Host {
    /// Registers a host and returns the event it produces.
    ///
    /// Registration happens when a worker first announces itself, so the
    /// initial status is `connected` and `last_seen_at_ms` is set. The id is
    /// freshly minted; a worker that wants to keep its identity across
    /// reconnects uses [`Host::register_as`].
    pub fn register(
        name: impl Into<String>,
        now_ms: u64,
    ) -> Result<(Self, DomainEvent), DomainError> {
        Self::register_as(None, name, now_ms)
    }

    /// Registers a host under an identity the caller already knows.
    ///
    /// A worker presents the id it was enrolled with so that a reconnect is a
    /// status change on the same host, not a second machine. `None` mints a
    /// fresh id, exactly like [`Host::register`].
    pub fn register_as(
        id: Option<HostId>,
        name: impl Into<String>,
        now_ms: u64,
    ) -> Result<(Self, DomainEvent), DomainError> {
        Self::register_with_data_dir(id, name, None, now_ms)
    }

    /// Registers a host together with the data directory its worker reported.
    ///
    /// Separate from [`Host::register_as`] so the many existing callers that
    /// do not know (or care) where a machine keeps its data stay unchanged.
    pub fn register_with_data_dir(
        id: Option<HostId>,
        name: impl Into<String>,
        data_dir: Option<String>,
        now_ms: u64,
    ) -> Result<(Self, DomainEvent), DomainError> {
        let name = name.into().trim().to_owned();
        if name.is_empty() {
            return Err(DomainError::InvalidField {
                field: "name",
                reason: "must not be empty".into(),
            });
        }
        let host = Self {
            id: id.unwrap_or_else(HostId::mint),
            name,
            kind: HostKind::Persistent,
            status: HostStatus::Connected,
            last_seen_at_ms: Some(now_ms),
            max_permission_mode: HostPermissionMode::Full,
            data_dir: data_dir
                .map(|dir| dir.trim().to_owned())
                .filter(|dir| !dir.is_empty()),
            created_at_ms: now_ms,
            updated_at_ms: now_ms,
        };
        let event = DomainEvent::HostRegistered { host: host.clone() };
        Ok((host, event))
    }

    /// Records a heartbeat without emitting an event; heartbeats are high
    /// frequency and not worth a frame.
    pub fn heartbeat(&mut self, now_ms: u64) {
        self.last_seen_at_ms = Some(now_ms);
        self.updated_at_ms = now_ms;
    }

    /// Records the data directory a worker reported, returning whether it
    /// changed.
    ///
    /// An enrollment is where a worker describes itself, and a worker that
    /// moves its data directory (a new `--state` path on the same machine)
    /// re-enrolls with the new value. Clearing it is not possible: an absent
    /// report leaves the recorded one alone, so a transient enrollment that
    /// omitted it cannot erase what a storage route depends on.
    pub fn record_data_dir(&mut self, data_dir: Option<&str>, now_ms: u64) -> bool {
        let Some(data_dir) = data_dir.map(str::trim).filter(|dir| !dir.is_empty()) else {
            return false;
        };
        if self.data_dir.as_deref() == Some(data_dir) {
            return false;
        }
        self.data_dir = Some(data_dir.to_owned());
        self.updated_at_ms = now_ms;
        true
    }

    /// Changes the display name.
    pub fn rename(&mut self, name: impl Into<String>, now_ms: u64) -> Result<(), DomainError> {
        let name = name.into().trim().to_owned();
        if name.is_empty() {
            return Err(DomainError::InvalidField {
                field: "name",
                reason: "must not be empty".into(),
            });
        }
        self.name = name;
        self.updated_at_ms = now_ms;
        Ok(())
    }

    /// Changes the permission ceiling without changing the worker connection.
    pub fn set_permission_ceiling(&mut self, mode: HostPermissionMode, now_ms: u64) {
        self.max_permission_mode = mode;
        self.updated_at_ms = now_ms;
    }

    /// Marks the worker attached, returning an event on an actual change.
    pub fn mark_connected(&mut self, now_ms: u64) -> Option<DomainEvent> {
        self.last_seen_at_ms = Some(now_ms);
        self.transition(HostStatus::Connected, now_ms)
    }

    /// Marks the worker detached, returning an event on an actual change.
    pub fn mark_disconnected(&mut self, now_ms: u64) -> Option<DomainEvent> {
        self.transition(HostStatus::Disconnected, now_ms)
    }

    fn transition(&mut self, to: HostStatus, now_ms: u64) -> Option<DomainEvent> {
        if self.status == to {
            self.updated_at_ms = now_ms;
            return None;
        }
        let from = self.status;
        self.status = to;
        self.updated_at_ms = now_ms;
        Some(DomainEvent::HostStatusChanged {
            host_id: self.id.clone(),
            from,
            to,
            at_ms: now_ms,
        })
    }
}

/// Chooses the host that a `"primary host"` query should use.
///
/// The rule exists to make server-only operation safe. bb's server falls back
/// to the *local* worker's id file, so a machine with no worker leaves file
/// browsing and host lookups stranded on a host that is intentionally absent.
/// Here the local host is only a *preference*:
/// 1. the declared local host, but only while a worker is actually attached to
///    it (status `connected`);
/// 2. otherwise the most recently seen connected host of any kind — the
///    primary simply falls to a remote execution machine;
/// 3. otherwise `None`, which is a normal "no host enrolled yet" answer.
///
/// It deliberately cannot fail, so no caller can surface a
/// `host_unavailable` error merely because this machine has no local worker.
pub fn select_primary_host<'a>(
    hosts: &'a [Host],
    local_host_id: Option<&HostId>,
) -> Option<&'a Host> {
    if let Some(local_host_id) = local_host_id {
        if let Some(host) = hosts
            .iter()
            .find(|host| &host.id == local_host_id && host.status == HostStatus::Connected)
        {
            return Some(host);
        }
    }
    hosts
        .iter()
        .filter(|host| host.status == HostStatus::Connected)
        .max_by_key(|host| host.last_seen_at_ms.unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connected(name: &str, id: Option<HostId>, seen_at: u64) -> Host {
        let (host, _) = Host::register_as(id, name, seen_at).unwrap();
        host
    }

    #[test]
    fn register_as_keeps_a_worker_supplied_identity() {
        let id = HostId::mint();
        let (host, _) = Host::register_as(Some(id.clone()), "laptop", 3).unwrap();
        assert_eq!(host.id, id);
    }

    #[test]
    fn a_declared_local_host_wins_while_it_is_connected() {
        let local_id = HostId::mint();
        let local = connected("local", Some(local_id.clone()), 1);
        let remote = connected("remote", None, 99);
        // Older than the remote host, but still the declared local one.
        let hosts = [local, remote];
        let selected = select_primary_host(&hosts, Some(&local_id)).unwrap();
        assert_eq!(selected.id, local_id);
    }

    #[test]
    fn an_absent_local_host_falls_back_to_a_remote_one() {
        let absent_local = HostId::mint();
        let remote = connected("remote", None, 5);
        let hosts = [remote.clone()];
        let selected = select_primary_host(&hosts, Some(&absent_local)).unwrap();
        assert_eq!(selected.id, remote.id);
    }

    #[test]
    fn a_disconnected_local_host_falls_back_to_a_remote_one() {
        let local_id = HostId::mint();
        let mut local = connected("local", Some(local_id.clone()), 1);
        local.mark_disconnected(2);
        let remote = connected("remote", None, 3);
        let hosts = [local, remote.clone()];
        let selected = select_primary_host(&hosts, Some(&local_id)).unwrap();
        assert_eq!(selected.id, remote.id);
    }

    #[test]
    fn no_connected_host_selects_nothing_instead_of_failing() {
        assert!(select_primary_host(&[], Some(&HostId::mint())).is_none());
        let mut only = connected("local", None, 1);
        only.mark_disconnected(2);
        let hosts = [only];
        assert!(select_primary_host(&hosts, None).is_none());
    }

    #[test]
    fn the_most_recently_seen_connected_host_wins() {
        let older = connected("older", None, 10);
        let newer = connected("newer", None, 20);
        let hosts = [older, newer.clone()];
        let selected = select_primary_host(&hosts, None).unwrap();
        assert_eq!(selected.id, newer.id);
    }

    #[test]
    fn register_rejects_an_empty_name() {
        assert!(matches!(
            Host::register("   ", 1),
            Err(DomainError::InvalidField { field: "name", .. })
        ));
    }

    #[test]
    fn register_starts_connected() {
        let (host, event) = Host::register("laptop", 5).unwrap();
        assert_eq!(host.status, HostStatus::Connected);
        assert_eq!(host.last_seen_at_ms, Some(5));
        assert!(matches!(event, DomainEvent::HostRegistered { .. }));
    }

    #[test]
    fn a_reported_data_directory_is_recorded_and_never_cleared_by_omission() {
        let (mut host, _) = Host::register("laptop", 1).unwrap();
        // A host that never reported one can never name thread storage.
        assert_eq!(host.data_dir, None);

        assert!(host.record_data_dir(Some("/var/lib/loom"), 2));
        assert_eq!(host.data_dir.as_deref(), Some("/var/lib/loom"));
        // The same value again is not a change, so no timestamp moves.
        assert!(!host.record_data_dir(Some("/var/lib/loom"), 3));
        assert_eq!(host.updated_at_ms, 2);

        // A worker that moved its data directory re-enrolls with the new value.
        assert!(host.record_data_dir(Some("/srv/loom"), 4));
        assert_eq!(host.data_dir.as_deref(), Some("/srv/loom"));

        // An enrollment that omits it must not erase a recorded layout: a
        // storage route would then stop answering for a machine that has not
        // moved anything.
        assert!(!host.record_data_dir(None, 5));
        assert!(!host.record_data_dir(Some("   "), 6));
        assert_eq!(host.data_dir.as_deref(), Some("/srv/loom"));
    }

    #[test]
    fn register_with_a_data_directory_carries_it_on_the_event() {
        let (host, event) =
            Host::register_with_data_dir(None, "laptop", Some("/var/lib/loom".into()), 7).unwrap();
        assert_eq!(host.data_dir.as_deref(), Some("/var/lib/loom"));
        match event {
            DomainEvent::HostRegistered { host: event_host } => {
                assert_eq!(event_host.data_dir.as_deref(), Some("/var/lib/loom"));
            }
            other => panic!("expected a registration, got {other:?}"),
        }
        // An empty report is normalised to `None` rather than stored as `""`.
        let (blank, _) =
            Host::register_with_data_dir(None, "laptop", Some("  ".into()), 8).unwrap();
        assert_eq!(blank.data_dir, None);
    }

    #[test]
    fn status_changes_only_emit_on_an_actual_change() {
        let (mut host, _) = Host::register("laptop", 5).unwrap();
        assert!(host.mark_connected(6).is_none());

        let event = host.mark_disconnected(7).unwrap();
        match event {
            DomainEvent::HostStatusChanged { from, to, .. } => {
                assert_eq!(from, HostStatus::Connected);
                assert_eq!(to, HostStatus::Disconnected);
            }
            other => panic!("expected a status change, got {other:?}"),
        }
        assert!(host.mark_disconnected(8).is_none());

        // A heartbeat still updates the timestamp without an event.
        host.heartbeat(9);
        assert_eq!(host.last_seen_at_ms, Some(9));
        assert_eq!(host.updated_at_ms, 9);
    }
}
