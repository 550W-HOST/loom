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

/// The provider that owns a managed environment's workspace as an empty
/// directory under the worker's workspace root.
pub const PERSONAL_WORKSPACE_PROVIDER_ID: &str = "personal-workspace";
/// The provider that owns a managed environment's workspace as a git worktree
/// cut from a project source.
pub const GIT_WORKTREE_PROVIDER_ID: &str = "git-worktree";
/// The provider that owns an unmanaged environment: a path the operator
/// already has.
pub const PROJECT_CHECKOUT_PROVIDER_ID: &str = "project-checkout";

/// How a caller asks for a managed environment's workspace.
///
/// The provider is a plain id rather than an enum so an unknown one is a
/// validation error instead of a parse failure, and `None` keeps the kind's
/// default. See [`EnvironmentKind::default_provider_id`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EnvironmentSelection {
    /// `git-worktree`, `personal-workspace` or `project-checkout`; `None`
    /// means the kind's default.
    pub provider_id: Option<String>,
    /// Base branch for a `git-worktree` environment; `None` means the
    /// source's default branch, as the worker resolves it.
    pub base_branch: Option<String>,
    /// Branch to check the worktree out on; `None` mints `loom/<env id>`.
    pub branch_name: Option<String>,
}

/// What a worker reports about the workspace it provisioned.
///
/// The path is the ownership record; branch and git facts are what the
/// contract's environment projection needs to stop reporting `null`.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct ProvisionedWorkspace {
    /// Absolute path the worker created.
    pub path: String,
    /// The branch the worktree is on, when the workspace is a git worktree.
    pub branch_name: Option<String>,
    /// The base branch the worktree was cut from.
    pub base_branch: Option<String>,
    /// The source repository's default branch, as the worker resolved it.
    pub default_branch: Option<String>,
    /// Whether the provisioned path is inside a git repository.
    pub is_git_repo: Option<bool>,
}

/// Where a managed environment's teardown is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvironmentTeardownStatus {
    /// The removal request is with the host.
    Running,
    /// The host refused or failed. The record stays `destroyed`, but the
    /// workspace may still exist and a retry is the remedy.
    Failed,
    /// The workspace is gone.
    Removed,
}

/// The state of a managed environment's teardown, recorded on the record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvironmentTeardown {
    /// Where the teardown is.
    pub status: EnvironmentTeardownStatus,
    /// Which attempt this is, counting from one. A retry increments it.
    #[serde(default)]
    pub attempt: u32,
    /// Why the last attempt failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// What a worker did with a teardown request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EnvironmentTeardownOutcome {
    /// The workspace is gone.
    Removed,
    /// The removal failed; the reason is recorded for the user.
    Failed(String),
}

/// Managed by loom, or pointing at a directory that already exists.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvironmentKind {
    /// Loom provisions the workspace and may clean it up.
    Managed,
    /// Points at an existing directory; loom never removes it.
    Unmanaged,
}

impl EnvironmentKind {
    /// The provider a create request means when it names none.
    pub fn default_provider_id(self) -> &'static str {
        match self {
            EnvironmentKind::Managed => PERSONAL_WORKSPACE_PROVIDER_ID,
            EnvironmentKind::Unmanaged => PROJECT_CHECKOUT_PROVIDER_ID,
        }
    }
}

/// Where an environment is in its provisioning life.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvironmentStatus {
    /// The record exists; provisioning has not been dispatched.
    Creating,
    /// A worker is preparing the workspace.
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
    /// The capability that owns the workspace: `git-worktree`,
    /// `personal-workspace` or `project-checkout`.
    ///
    /// `None` is a record from before the field existed; callers treat it as
    /// the kind's default. Stored explicitly from the moment of creation so a
    /// client can tell a personal workspace from a worktree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_id: Option<String>,
    /// The branch used when comparing this workspace with its project.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge_base_branch: Option<String>,
    /// The branch a managed worktree was cut from, once it is provisioned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_branch: Option<String>,
    /// The branch a managed worktree is checked out on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch_name: Option<String>,
    /// The source repository's default branch, as the worker observed it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_branch: Option<String>,
    /// Whether the provisioned path is inside a git repository.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_git_repo: Option<bool>,
    /// The state of a managed environment's teardown, once one started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub teardown: Option<EnvironmentTeardown>,
    /// Absolute workspace path. `None` until a managed environment is
    /// provisioned; always `Some` for an unmanaged one.
    pub path: Option<String>,
    /// Provisioning status.
    pub status: EnvironmentStatus,
    /// Why the last provisioning attempt failed, when `status` is `error`.
    ///
    /// Carried so the worker's reason reaches a client instead of being
    /// reduced to a bare status. Cleared by the next legal transition out of
    /// `error`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Wall-clock milliseconds when the environment was created.
    pub created_at_ms: u64,
    /// Wall-clock milliseconds of the last mutation.
    pub updated_at_ms: u64,
}

impl Environment {
    /// Creates an environment with the default provider for its kind.
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
        Self::create_with(
            project_id,
            host_id,
            kind,
            path,
            EnvironmentSelection::default(),
            now_ms,
        )
    }

    /// Creates an environment with an explicit provider selection.
    ///
    /// The selection is validated against the kind: `git-worktree` and
    /// `personal-workspace` are managed, `project-checkout` is the unmanaged
    /// one, and an unknown provider id is rejected rather than stored. A
    /// `git-worktree` environment gets the caller's branch name or the minted
    /// `loom/<environment id>` default; a branch selection on any other
    /// provider is rejected.
    pub fn create_with(
        project_id: ProjectId,
        host_id: HostId,
        kind: EnvironmentKind,
        path: Option<String>,
        selection: EnvironmentSelection,
        now_ms: u64,
    ) -> Result<(Self, DomainEvent), DomainError> {
        let path = path
            .map(|path| path.trim().to_owned())
            .filter(|path| !path.is_empty());

        let provider_id = selection
            .provider_id
            .as_deref()
            .map(str::trim)
            .filter(|provider| !provider.is_empty())
            .unwrap_or_else(|| kind.default_provider_id())
            .to_owned();
        if !matches!(
            provider_id.as_str(),
            GIT_WORKTREE_PROVIDER_ID
                | PERSONAL_WORKSPACE_PROVIDER_ID
                | PROJECT_CHECKOUT_PROVIDER_ID
        ) {
            return Err(DomainError::InvalidField {
                field: "provider_id",
                reason: format!("{provider_id} is not a known environment provider"),
            });
        }
        match (kind, provider_id.as_str()) {
            (EnvironmentKind::Managed, GIT_WORKTREE_PROVIDER_ID)
            | (EnvironmentKind::Managed, PERSONAL_WORKSPACE_PROVIDER_ID)
            | (EnvironmentKind::Unmanaged, PROJECT_CHECKOUT_PROVIDER_ID) => {}
            (EnvironmentKind::Managed, _) => {
                return Err(DomainError::InvalidField {
                    field: "provider_id",
                    reason: format!("{provider_id} is not a managed provider"),
                })
            }
            (EnvironmentKind::Unmanaged, _) => {
                return Err(DomainError::InvalidField {
                    field: "provider_id",
                    reason: format!("{provider_id} cannot own an unmanaged environment"),
                })
            }
        }
        let base_branch = normalize_branch_selection(selection.base_branch, "base_branch")?;
        let mut branch_name = normalize_branch_selection(selection.branch_name, "branch_name")?;
        if provider_id != GIT_WORKTREE_PROVIDER_ID
            && (base_branch.is_some() || branch_name.is_some())
        {
            return Err(DomainError::InvalidField {
                field: "branch_name",
                reason: "only git-worktree takes a branch selection".into(),
            });
        }

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

        let id = EnvironmentId::mint();
        if provider_id == GIT_WORKTREE_PROVIDER_ID && branch_name.is_none() {
            branch_name = Some(format!("loom/{id}"));
        }
        let environment = Self {
            id,
            name: None,
            project_id,
            host_id,
            kind,
            provider_id: Some(provider_id),
            merge_base_branch: None,
            base_branch,
            branch_name,
            default_branch: None,
            is_git_repo: None,
            teardown: None,
            path,
            status,
            error: None,
            created_at_ms: now_ms,
            updated_at_ms: now_ms,
        };
        let event = DomainEvent::EnvironmentCreated {
            environment: environment.clone(),
        };
        Ok((environment, event))
    }

    /// Whether this environment's workspace is a managed git worktree.
    pub fn is_worktree(&self) -> bool {
        self.provider_id.as_deref() == Some(GIT_WORKTREE_PROVIDER_ID)
    }

    /// Updates the display name and/or merge-base branch.
    ///
    /// The double options distinguish an omitted field from an explicit null,
    /// matching the PATCH contract. Updating an environment is observable on
    /// the project scope as a whole-value event.
    pub fn update(
        &mut self,
        name: Option<Option<String>>,
        merge_base_branch: Option<Option<String>>,
        now_ms: u64,
    ) -> Result<DomainEvent, DomainError> {
        // Normalize and validate the whole patch before touching `self`. A
        // malformed second field must not leave the first field committed.
        let name = name.map(|name| name.map(|name| name.trim().to_owned()));
        if name.as_ref().and_then(Option::as_deref) == Some("") {
            return Err(DomainError::InvalidField {
                field: "name",
                reason: "must not be empty".into(),
            });
        }
        let merge_base_branch =
            merge_base_branch.map(|branch| branch.map(|branch| branch.trim().to_owned()));
        if merge_base_branch.as_ref().and_then(Option::as_deref) == Some("") {
            return Err(DomainError::InvalidField {
                field: "merge_base_branch",
                reason: "must not be empty".into(),
            });
        }

        if let Some(name) = name {
            self.name = name;
        }
        if let Some(branch) = merge_base_branch {
            self.merge_base_branch = branch;
        }
        self.updated_at_ms = now_ms;
        Ok(DomainEvent::EnvironmentUpdated {
            environment: self.clone(),
        })
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
        if to != EnvironmentStatus::Error {
            self.error = None;
        }
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

    /// Moves the environment to `error` with the reason a provisioning
    /// attempt failed.
    ///
    /// The reason is recorded alongside the status so a client can render it;
    /// the transition itself is the ordinary `-> error` one and is rejected
    /// when it is not legal (a destroyed environment, for example).
    pub fn set_error(
        &mut self,
        reason: impl Into<String>,
        now_ms: u64,
    ) -> Result<DomainEvent, DomainError> {
        let event = self.set_status(EnvironmentStatus::Error, now_ms)?;
        self.error = Some(reason.into());
        Ok(event)
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

    /// Records what a worker provisioned and advances the environment to
    /// `ready` as one state-machine operation.
    ///
    /// Returns two events: the status change a client watches, and the
    /// whole-environment update that makes `path`, branch and git facts
    /// durable through replay — `EnvironmentStatusChanged` carries only the
    /// status, so a restart that recovers by replay would otherwise lose the
    /// ownership record teardown needs.
    pub fn complete_provisioning(
        &mut self,
        workspace: ProvisionedWorkspace,
        now_ms: u64,
    ) -> Result<Vec<DomainEvent>, DomainError> {
        if self.status != EnvironmentStatus::Provisioning {
            return Err(DomainError::IllegalEnvironmentTransition {
                from: self.status,
                to: EnvironmentStatus::Ready,
            });
        }
        self.set_provisioned_path(workspace.path, now_ms)?;
        if workspace.branch_name.is_some() {
            self.branch_name = workspace.branch_name;
        }
        if workspace.base_branch.is_some() {
            self.base_branch = workspace.base_branch;
        }
        if workspace.default_branch.is_some() {
            self.default_branch = workspace.default_branch;
        }
        if workspace.is_git_repo.is_some() {
            self.is_git_repo = workspace.is_git_repo;
        }
        let status = self.set_status(EnvironmentStatus::Ready, now_ms)?;
        let updated = DomainEvent::EnvironmentUpdated {
            environment: self.clone(),
        };
        Ok(vec![status, updated])
    }

    /// Moves the environment to `destroyed` and records an in-flight teardown.
    ///
    /// Returns the status event a client watches and the whole-environment
    /// update that makes the teardown record durable. Destroying an
    /// already-destroyed environment is a retry, not an error: it increments
    /// the attempt and resets the record to `running`.
    pub fn begin_teardown(&mut self, now_ms: u64) -> Result<Vec<DomainEvent>, DomainError> {
        let mut events = Vec::new();
        if self.status != EnvironmentStatus::Destroyed {
            events.push(self.set_status(EnvironmentStatus::Destroyed, now_ms)?);
        }
        let attempt = self
            .teardown
            .as_ref()
            .map_or(0, |teardown| teardown.attempt)
            + 1;
        self.teardown = Some(EnvironmentTeardown {
            status: EnvironmentTeardownStatus::Running,
            attempt,
            message: None,
        });
        self.updated_at_ms = now_ms;
        events.push(DomainEvent::EnvironmentUpdated {
            environment: self.clone(),
        });
        Ok(events)
    }

    /// Records the outcome of the in-flight teardown.
    ///
    /// A report for a teardown that is not running is refused: redelivery of an
    /// already settled removal is the caller's cue to treat it as stale.
    pub fn complete_teardown(
        &mut self,
        outcome: EnvironmentTeardownOutcome,
        now_ms: u64,
    ) -> Result<DomainEvent, DomainError> {
        let Some(teardown) = self.teardown.as_mut() else {
            return Err(DomainError::InvalidField {
                field: "teardown",
                reason: "no teardown is in flight".into(),
            });
        };
        if teardown.status != EnvironmentTeardownStatus::Running {
            return Err(DomainError::InvalidField {
                field: "teardown",
                reason: "the teardown is already settled".into(),
            });
        }
        match outcome {
            EnvironmentTeardownOutcome::Removed => {
                teardown.status = EnvironmentTeardownStatus::Removed;
                teardown.message = None;
            }
            EnvironmentTeardownOutcome::Failed(message) => {
                teardown.status = EnvironmentTeardownStatus::Failed;
                teardown.message = Some(message);
            }
        }
        self.updated_at_ms = now_ms;
        Ok(DomainEvent::EnvironmentUpdated {
            environment: self.clone(),
        })
    }
}

/// Trims and validates an optional branch selection.
///
/// An empty or whitespace-only value is `None`; anything else must look like a
/// git branch. The rules mirror the worker's `validate_branch_reference` so a
/// name accepted here does not fail provisioning later.
fn normalize_branch_selection(
    value: Option<String>,
    field: &'static str,
) -> Result<Option<String>, DomainError> {
    let Some(value) = value
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
    else {
        return Ok(None);
    };
    if let Err(reason) = validate_branch_name(&value) {
        return Err(DomainError::InvalidField {
            field,
            reason: reason.into(),
        });
    }
    Ok(Some(value))
}

/// Whether `raw` is a branch name git would accept as a fresh reference.
///
/// Deliberately conservative and dependency-free: the worker is the final
/// authority because it runs git, but a name that cannot possibly work should
/// not create an environment in the first place.
fn validate_branch_name(raw: &str) -> Result<(), &'static str> {
    if raw.is_empty() {
        return Err("branch is empty");
    }
    if raw.len() > 4_096 {
        return Err("branch is too long");
    }
    if raw == "@" || raw.starts_with('-') {
        return Err("branch is not a valid reference");
    }
    if raw
        .chars()
        .any(|character| character.is_whitespace() || character.is_control())
    {
        return Err("branch contains whitespace or control characters");
    }
    if raw.contains("..") || raw.contains("@{") || raw.starts_with('/') || raw.ends_with('/') {
        return Err("branch contains a forbidden reference sequence");
    }
    if raw
        .bytes()
        .any(|byte| matches!(byte, b'~' | b'^' | b':' | b'?' | b'*' | b'[' | b'\\'))
    {
        return Err("branch contains a forbidden reference character");
    }
    if raw.split('/').any(|component| {
        component.is_empty()
            || component == "."
            || component == ".."
            || component.starts_with('.')
            || component.ends_with('.')
            || component.ends_with(".lock")
    }) {
        return Err("branch contains an invalid reference component");
    }
    Ok(())
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

    #[test]
    fn a_failed_provision_records_its_reason_and_clears_it_on_retry() {
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
        managed.set_error("could not create /srv/work", 3).unwrap();
        assert_eq!(managed.status, EnvironmentStatus::Error);
        assert_eq!(managed.error.as_deref(), Some("could not create /srv/work"));

        // `error -> provisioning` is legal: a retry starts clean.
        managed
            .set_status(EnvironmentStatus::Provisioning, 4)
            .unwrap();
        assert_eq!(managed.error, None);
    }

    #[test]
    fn an_invalid_field_does_not_partially_apply_an_environment_update() {
        let (mut environment, _) = Environment::create(
            ProjectId::mint(),
            HostId::mint(),
            EnvironmentKind::Unmanaged,
            Some("/srv/loom".into()),
            1,
        )
        .unwrap();
        environment
            .update(Some(Some("before".into())), Some(Some("main".into())), 2)
            .unwrap();

        let result = environment.update(Some(Some("after".into())), Some(Some("   ".into())), 3);
        assert!(matches!(
            result,
            Err(DomainError::InvalidField {
                field: "merge_base_branch",
                ..
            })
        ));
        assert_eq!(environment.name.as_deref(), Some("before"));
        assert_eq!(environment.merge_base_branch.as_deref(), Some("main"));
        assert_eq!(environment.updated_at_ms, 2);
    }

    #[test]
    fn a_destroyed_environment_rejects_a_late_failure_report() {
        let (mut unmanaged, _) = Environment::create(
            ProjectId::mint(),
            HostId::mint(),
            EnvironmentKind::Unmanaged,
            Some("/srv/loom".into()),
            1,
        )
        .unwrap();
        unmanaged
            .set_status(EnvironmentStatus::Destroyed, 2)
            .unwrap();
        assert!(matches!(
            unmanaged.set_error("too late", 3),
            Err(DomainError::IllegalEnvironmentTransition { .. })
        ));
        assert_eq!(unmanaged.status, EnvironmentStatus::Destroyed);
        assert_eq!(unmanaged.error, None);
    }

    #[test]
    fn completing_provisioning_is_atomic_when_the_path_is_invalid() {
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

        assert!(managed
            .complete_provisioning(
                ProvisionedWorkspace {
                    path: "   ".into(),
                    ..ProvisionedWorkspace::default()
                },
                3,
            )
            .is_err());
        assert_eq!(managed.status, EnvironmentStatus::Provisioning);
        assert_eq!(managed.path, None);
        assert_eq!(managed.updated_at_ms, 2);
    }

    #[test]
    fn a_managed_environment_defaults_to_the_personal_provider() {
        let (managed, _) = Environment::create(
            ProjectId::mint(),
            HostId::mint(),
            EnvironmentKind::Managed,
            None,
            1,
        )
        .unwrap();
        assert_eq!(
            managed.provider_id.as_deref(),
            Some(PERSONAL_WORKSPACE_PROVIDER_ID)
        );
        assert!(!managed.is_worktree());

        let (unmanaged, _) = Environment::create(
            ProjectId::mint(),
            HostId::mint(),
            EnvironmentKind::Unmanaged,
            Some("/srv/loom".into()),
            1,
        )
        .unwrap();
        assert_eq!(
            unmanaged.provider_id.as_deref(),
            Some(PROJECT_CHECKOUT_PROVIDER_ID)
        );
    }

    #[test]
    fn a_worktree_mints_its_branch_when_none_is_named() {
        let (worktree, _) = Environment::create_with(
            ProjectId::mint(),
            HostId::mint(),
            EnvironmentKind::Managed,
            None,
            EnvironmentSelection {
                provider_id: Some(GIT_WORKTREE_PROVIDER_ID.into()),
                ..EnvironmentSelection::default()
            },
            1,
        )
        .unwrap();
        assert!(worktree.is_worktree());
        assert_eq!(
            worktree.branch_name.as_deref(),
            Some(format!("loom/{}", worktree.id).as_str())
        );
        assert_eq!(worktree.base_branch, None);
    }

    #[test]
    fn a_worktree_keeps_a_named_branch_and_base() {
        let (worktree, _) = Environment::create_with(
            ProjectId::mint(),
            HostId::mint(),
            EnvironmentKind::Managed,
            None,
            EnvironmentSelection {
                provider_id: Some(GIT_WORKTREE_PROVIDER_ID.into()),
                base_branch: Some("origin/main".into()),
                branch_name: Some("feat/one".into()),
            },
            1,
        )
        .unwrap();
        assert_eq!(worktree.base_branch.as_deref(), Some("origin/main"));
        assert_eq!(worktree.branch_name.as_deref(), Some("feat/one"));
    }

    #[test]
    fn a_branch_selection_is_refused_outside_git_worktree() {
        for provider in [PERSONAL_WORKSPACE_PROVIDER_ID, PROJECT_CHECKOUT_PROVIDER_ID] {
            let kind = if provider == PROJECT_CHECKOUT_PROVIDER_ID {
                EnvironmentKind::Unmanaged
            } else {
                EnvironmentKind::Managed
            };
            let path = (kind == EnvironmentKind::Unmanaged).then(|| "/srv/loom".into());
            assert!(matches!(
                Environment::create_with(
                    ProjectId::mint(),
                    HostId::mint(),
                    kind,
                    path,
                    EnvironmentSelection {
                        provider_id: Some(provider.into()),
                        base_branch: Some("main".into()),
                        branch_name: None,
                    },
                    1,
                ),
                Err(DomainError::InvalidField {
                    field: "branch_name",
                    ..
                })
            ));
        }
    }

    #[test]
    fn an_unknown_provider_is_refused() {
        assert!(matches!(
            Environment::create_with(
                ProjectId::mint(),
                HostId::mint(),
                EnvironmentKind::Managed,
                None,
                EnvironmentSelection {
                    provider_id: Some("docker".into()),
                    ..EnvironmentSelection::default()
                },
                1,
            ),
            Err(DomainError::InvalidField {
                field: "provider_id",
                ..
            })
        ));
    }

    #[test]
    fn completion_records_branch_and_git_facts_with_a_durable_event() {
        let (mut worktree, _) = Environment::create_with(
            ProjectId::mint(),
            HostId::mint(),
            EnvironmentKind::Managed,
            None,
            EnvironmentSelection {
                provider_id: Some(GIT_WORKTREE_PROVIDER_ID.into()),
                base_branch: Some("main".into()),
                branch_name: None,
            },
            1,
        )
        .unwrap();
        worktree
            .set_status(EnvironmentStatus::Provisioning, 2)
            .unwrap();
        let events = worktree
            .complete_provisioning(
                ProvisionedWorkspace {
                    path: "/root/env_1".into(),
                    branch_name: Some(format!("loom/{}", worktree.id)),
                    base_branch: Some("origin/main".into()),
                    default_branch: Some("main".into()),
                    is_git_repo: Some(true),
                },
                3,
            )
            .unwrap();
        assert_eq!(worktree.status, EnvironmentStatus::Ready);
        assert_eq!(worktree.path.as_deref(), Some("/root/env_1"));
        assert_eq!(worktree.base_branch.as_deref(), Some("origin/main"));
        assert_eq!(worktree.default_branch.as_deref(), Some("main"));
        assert_eq!(worktree.is_git_repo, Some(true));
        assert!(matches!(
            events.as_slice(),
            [
                DomainEvent::EnvironmentStatusChanged {
                    to: EnvironmentStatus::Ready,
                    ..
                },
                DomainEvent::EnvironmentUpdated { .. }
            ]
        ));
        let DomainEvent::EnvironmentUpdated { environment } = &events[1] else {
            unreachable!("checked above")
        };
        assert_eq!(environment.path.as_deref(), Some("/root/env_1"));
        assert_eq!(environment.branch_name, worktree.branch_name);
    }

    #[test]
    fn teardown_starts_running_settles_and_a_retry_increments_the_attempt() {
        let (mut unmanaged, _) = Environment::create(
            ProjectId::mint(),
            HostId::mint(),
            EnvironmentKind::Unmanaged,
            Some("/srv/loom".into()),
            1,
        )
        .unwrap();
        let events = unmanaged.begin_teardown(2).unwrap();
        assert_eq!(unmanaged.status, EnvironmentStatus::Destroyed);
        assert_eq!(
            unmanaged.teardown,
            Some(EnvironmentTeardown {
                status: EnvironmentTeardownStatus::Running,
                attempt: 1,
                message: None,
            })
        );
        assert!(matches!(
            events.as_slice(),
            [
                DomainEvent::EnvironmentStatusChanged {
                    to: EnvironmentStatus::Destroyed,
                    ..
                },
                DomainEvent::EnvironmentUpdated { .. }
            ]
        ));

        let event = unmanaged
            .complete_teardown(EnvironmentTeardownOutcome::Failed("locked".into()), 3)
            .unwrap();
        let DomainEvent::EnvironmentUpdated { environment } = event else {
            panic!("a teardown outcome is a whole-environment update");
        };
        assert_eq!(
            environment.teardown.as_ref().map(|t| t.status),
            Some(EnvironmentTeardownStatus::Failed)
        );
        assert_eq!(
            environment
                .teardown
                .as_ref()
                .and_then(|t| t.message.as_deref()),
            Some("locked")
        );

        // Destroying a destroyed environment tears down again; the attempt
        // count is what a UI shows and what a retry keys on.
        let events = unmanaged.begin_teardown(4).unwrap();
        assert!(matches!(
            events.as_slice(),
            [DomainEvent::EnvironmentUpdated { .. }]
        ));
        assert_eq!(unmanaged.teardown.as_ref().map(|t| t.attempt), Some(2));
        unmanaged
            .complete_teardown(EnvironmentTeardownOutcome::Removed, 5)
            .unwrap();
        assert_eq!(
            unmanaged.teardown.as_ref().map(|t| t.status),
            Some(EnvironmentTeardownStatus::Removed)
        );
        // A redelivered settlement is refused, not applied twice.
        assert!(unmanaged
            .complete_teardown(EnvironmentTeardownOutcome::Removed, 6)
            .is_err());
    }
}
