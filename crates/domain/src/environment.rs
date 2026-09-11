//! Environments: a thread's execution context.
//!
//! An environment binds a workspace (a directory on disk) to a host, and is
//! either **managed** (loom provisions and later cleans up the directory) or
//! **unmanaged** (it points at a directory the operator already has). That
//! distinction is encoded in [`EnvironmentKind`] and validated at creation:
//! a managed environment has no path yet, an unmanaged one requires one.

use serde::{Deserialize, Serialize};

use crate::error::DomainError;
use crate::event::DomainEvent;
use crate::id::{EnvironmentId, HostId, ProjectId};

/// Managed by loom, or pointing at a directory that already exists.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvironmentKind {
    /// Loom provisions the workspace and may clean it up.
    Managed,
    /// Points at an existing directory; loom never removes it.
    Unmanaged,
}

/// Where an environment is in its provisioning life.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvironmentStatus {
    /// The record exists; provisioning has not been dispatched.
    Creating,
    /// A daemon is preparing the workspace.
    Provisioning,
    /// The workspace is usable.
    Ready,
    /// Provisioning failed; a retry or teardown is required.
    Error,
    /// The workspace has been torn down. Terminal.
    Destroyed,
}

impl EnvironmentStatus {
    /// Every status, in lifecycle order.
    pub const ALL: [EnvironmentStatus; 5] = [
        EnvironmentStatus::Creating,
        EnvironmentStatus::Provisioning,
        EnvironmentStatus::Ready,
        EnvironmentStatus::Error,
        EnvironmentStatus::Destroyed,
    ];

    /// Whether `from -> to` is a legal transition.
    ///
    /// `destroyed` is reachable from every live status (teardown is always
    /// possible) and is terminal.
    pub fn can_transition(from: EnvironmentStatus, to: EnvironmentStatus) -> bool {
        use EnvironmentStatus::{Creating, Destroyed, Error, Provisioning, Ready};
        if from == Destroyed {
            return false;
        }
        match (from, to) {
            (_, Destroyed) => true,
            (Creating, Provisioning | Ready | Error) => true,
            (Provisioning, Ready | Error) => true,
            (Ready, _) => false,
            (Error, Provisioning) => true,
            _ => false,
        }
    }
}

impl std::fmt::Display for EnvironmentStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            EnvironmentStatus::Creating => "creating",
            EnvironmentStatus::Provisioning => "provisioning",
            EnvironmentStatus::Ready => "ready",
            EnvironmentStatus::Error => "error",
            EnvironmentStatus::Destroyed => "destroyed",
        };
        f.write_str(name)
    }
}

/// A thread's execution context: workspace plus host.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Environment {
    /// Identity.
    pub id: EnvironmentId,
    /// Optional display name.
    pub name: Option<String>,
    /// The owning project.
    pub project_id: ProjectId,
    /// The machine the workspace lives on.
    pub host_id: HostId,
    /// Managed or unmanaged.
    pub kind: EnvironmentKind,
    /// Absolute workspace path. `None` until a managed environment is
    /// provisioned; always `Some` for an unmanaged one.
    pub path: Option<String>,
    /// Provisioning status.
    pub status: EnvironmentStatus,
    /// Wall-clock milliseconds when the environment was created.
    pub created_at_ms: u64,
    /// Wall-clock milliseconds of the last mutation.
    pub updated_at_ms: u64,
}

impl Environment {
    /// Creates an environment and the event it produces.
    ///
    /// An unmanaged environment must name an existing path and starts `ready`.
    /// A managed one must not name a path yet and starts `creating`.
    pub fn create(
        project_id: ProjectId,
        host_id: HostId,
        kind: EnvironmentKind,
        path: Option<String>,
        now_ms: u64,
    ) -> Result<(Self, DomainEvent), DomainError> {
        let path = path
            .map(|path| path.trim().to_owned())
            .filter(|path| !path.is_empty());

        let (path, status) = match (kind, path) {
            (EnvironmentKind::Unmanaged, None) => {
                return Err(DomainError::InvalidField {
                    field: "path",
                    reason: "an unmanaged environment must name an existing path".into(),
                })
            }
            (EnvironmentKind::Managed, Some(_)) => {
                return Err(DomainError::InvalidField {
                    field: "path",
                    reason: "a managed environment has no path until it is provisioned".into(),
                })
            }
            (EnvironmentKind::Managed, None) => (None, EnvironmentStatus::Creating),
            (EnvironmentKind::Unmanaged, Some(path)) => (Some(path), EnvironmentStatus::Ready),
        };

        let environment = Self {
            id: EnvironmentId::mint(),
            name: None,
            project_id,
            host_id,
            kind,
            path,
            status,
            created_at_ms: now_ms,
            updated_at_ms: now_ms,
        };
        let event = DomainEvent::EnvironmentCreated {
            environment: environment.clone(),
        };
        Ok((environment, event))
    }

    /// Moves the environment to `to` and returns the status-change event.
    ///
    /// The status is left untouched when the transition is illegal.
    pub fn set_status(
        &mut self,
        to: EnvironmentStatus,
        now_ms: u64,
    ) -> Result<DomainEvent, DomainError> {
        let from = self.status;
        if !EnvironmentStatus::can_transition(from, to) {
            return Err(DomainError::IllegalEnvironmentTransition { from, to });
        }
        self.status = to;
        self.updated_at_ms = now_ms;
        Ok(DomainEvent::EnvironmentStatusChanged {
            environment_id: self.id.clone(),
            project_id: self.project_id.clone(),
            host_id: self.host_id.clone(),
            from,
            to,
            at_ms: now_ms,
        })
    }

    /// Records the path once a managed environment has been provisioned.
    ///
    /// This does not emit an event on its own; it is part of the
    /// `provisioning -> ready` transition.
    pub fn set_provisioned_path(
        &mut self,
        path: impl Into<String>,
        now_ms: u64,
    ) -> Result<(), DomainError> {
        let path = path.into().trim().to_owned();
        if path.is_empty() {
            return Err(DomainError::InvalidField {
                field: "path",
                reason: "must not be empty".into(),
            });
        }
        self.path = Some(path);
        self.updated_at_ms = now_ms;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unmanaged_environment_requires_a_path() {
        assert!(matches!(
            Environment::create(
                ProjectId::mint(),
                HostId::mint(),
                EnvironmentKind::Unmanaged,
                None,
                1,
            ),
            Err(DomainError::InvalidField { field: "path", .. })
        ));
    }

    #[test]
    fn a_managed_environment_must_not_name_a_path() {
        assert!(matches!(
            Environment::create(
                ProjectId::mint(),
                HostId::mint(),
                EnvironmentKind::Managed,
                Some("/tmp/x".into()),
                1,
            ),
            Err(DomainError::InvalidField { field: "path", .. })
        ));
    }

    #[test]
    fn the_two_kinds_start_in_different_states() {
        let (managed, _) = Environment::create(
            ProjectId::mint(),
            HostId::mint(),
            EnvironmentKind::Managed,
            None,
            1,
        )
        .unwrap();
        assert_eq!(managed.status, EnvironmentStatus::Creating);
        assert_eq!(managed.path, None);

        let (unmanaged, event) = Environment::create(
            ProjectId::mint(),
            HostId::mint(),
            EnvironmentKind::Unmanaged,
            Some("/srv/loom".into()),
            1,
        )
        .unwrap();
        assert_eq!(unmanaged.status, EnvironmentStatus::Ready);
        assert_eq!(unmanaged.path.as_deref(), Some("/srv/loom"));
        assert!(matches!(event, DomainEvent::EnvironmentCreated { .. }));
    }

    #[test]
    fn provisioning_reaches_ready_and_destroyed_is_terminal() {
        let (mut managed, _) = Environment::create(
            ProjectId::mint(),
            HostId::mint(),
            EnvironmentKind::Managed,
            None,
            1,
        )
        .unwrap();

        managed
            .set_status(EnvironmentStatus::Provisioning, 2)
            .unwrap();
        managed.set_provisioned_path("/srv/work", 3).unwrap();
        managed.set_status(EnvironmentStatus::Ready, 4).unwrap();
        assert_eq!(managed.status, EnvironmentStatus::Ready);
        assert_eq!(managed.path.as_deref(), Some("/srv/work"));

        managed.set_status(EnvironmentStatus::Destroyed, 5).unwrap();
        assert!(matches!(
            managed.set_status(EnvironmentStatus::Ready, 6),
            Err(DomainError::IllegalEnvironmentTransition { .. })
        ));
    }

    #[test]
    fn ready_cannot_go_back_to_provisioning() {
        let (mut unmanaged, _) = Environment::create(
            ProjectId::mint(),
            HostId::mint(),
            EnvironmentKind::Unmanaged,
            Some("/srv/loom".into()),
            1,
        )
        .unwrap();
        assert!(matches!(
            unmanaged.set_status(EnvironmentStatus::Provisioning, 2),
            Err(DomainError::IllegalEnvironmentTransition { .. })
        ));
        assert_eq!(unmanaged.status, EnvironmentStatus::Ready);
    }
}
