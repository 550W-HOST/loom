//! Projects and their sources.
//!
//! A project is the top-level container, usually one repository. Its
//! [`ProjectSource`]s say where the code lives: one project can map to paths on
//! several hosts, one source per host, and a source may also name the git
//! remote it is a checkout of.
//!
//! # Shape decisions
//!
//! * **A source is a per-host location.** It names the host it lives on and,
//!   when the code is a checkout, the git remote it came from
//!   ([`ProjectSource::git_remote_url`]). Declaring a source never clones or
//!   fetches: actually materialising a workspace is the environment's job.
//! * **The personal project is ordinary after startup.** The server seeds
//!   exactly one [`ProjectKind::Personal`] project when it first starts and
//!   then treats it like any other project — it is listed, renamed and sourced
//!   normally. It is **not** a hidden fallback: a thread must name the project
//!   it belongs to. See `docs/projects.md`.
//! * **Archiving is refused while a run is in flight.** A project with a
//!   thread in `working` or `waiting` cannot be archived. Idle, errored and
//!   archived threads do not block it and are **not** cascaded: they keep
//!   their project reference. See [`Project::archive`].
//!
//! Every mutation rejects an archived project and returns the
//! [`DomainEvent::ProjectUpdated`] it produced, so a project's whole lifecycle
//! is observable on its `project:{id}` scope.

use serde::{Deserialize, Serialize};

use crate::error::DomainError;
use crate::event::DomainEvent;
use crate::id::{HostId, ProjectId, ProjectSourceId};

/// What a project is for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectKind {
    /// A normal, user-created project.
    #[default]
    Standard,
    /// The workspace's seeded project.
    ///
    /// It marks provenance, not privilege: after startup it behaves exactly
    /// like a [`ProjectKind::Standard`] one.
    Personal,
}

/// Where a project's code lives.
///
/// A source always names the [`HostId`] the location is on. When the source is
/// a checkout, [`ProjectSource::git_remote_url`] records the remote it came
/// from; loom does not clone or fetch, it only records the declaration.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectSource {
    /// Identity.
    pub id: ProjectSourceId,
    /// The owning project.
    pub project_id: ProjectId,
    /// The enrolled host this location belongs to.
    pub host_id: HostId,
    /// Absolute path on that host. Empty only for a remote-only source that
    /// declares a repository before any checkout exists.
    pub path: String,
    /// The git remote this location is a checkout of, when there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_remote_url: Option<String>,
    /// Whether this is the project's default source.
    pub is_default: bool,
    /// Wall-clock milliseconds when the source was added.
    pub created_at_ms: u64,
    /// Wall-clock milliseconds of the last mutation.
    pub updated_at_ms: u64,
}

/// The top-level container.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Project {
    /// Identity.
    pub id: ProjectId,
    /// Standard or personal.
    pub kind: ProjectKind,
    /// Display name.
    pub name: String,
    /// The repository remote, when the project is backed by one.
    pub git_remote_url: Option<String>,
    /// Where the code lives, one entry per host.
    pub sources: Vec<ProjectSource>,
    /// When the project was archived, if it is. `None` means active.
    ///
    /// Archiving is a lifecycle end, not a delete: the record stays so threads,
    /// environments and events that reference it keep resolving.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived_at_ms: Option<u64>,
    /// Wall-clock milliseconds when the project was created.
    pub created_at_ms: u64,
    /// Wall-clock milliseconds of the last mutation.
    pub updated_at_ms: u64,
}

impl Project {
    /// Creates a project with no sources, and the event it produces.
    pub fn create(
        name: impl Into<String>,
        kind: ProjectKind,
        now_ms: u64,
    ) -> Result<(Self, DomainEvent), DomainError> {
        Self::create_with_remote(name, kind, None, now_ms)
    }

    /// Creates a project, optionally backed by a git remote.
    ///
    /// One event, not two: a project created with a remote is announced by a
    /// single `project_created` carrying the remote, rather than a create
    /// followed by an update.
    pub fn create_with_remote(
        name: impl Into<String>,
        kind: ProjectKind,
        git_remote_url: Option<String>,
        now_ms: u64,
    ) -> Result<(Self, DomainEvent), DomainError> {
        let name = name.into().trim().to_owned();
        if name.is_empty() {
            return Err(DomainError::InvalidField {
                field: "name",
                reason: "must not be empty".into(),
            });
        }
        let project = Self {
            id: ProjectId::mint(),
            kind,
            name,
            git_remote_url: normalise_remote(git_remote_url),
            sources: Vec::new(),
            archived_at_ms: None,
            created_at_ms: now_ms,
            updated_at_ms: now_ms,
        };
        let event = DomainEvent::ProjectCreated {
            project: project.clone(),
        };
        Ok((project, event))
    }

    /// Whether the project is archived, and therefore read-only.
    pub fn is_archived(&self) -> bool {
        self.archived_at_ms.is_some()
    }

    /// Renames the project and returns the update event.
    pub fn rename(
        &mut self,
        name: impl Into<String>,
        now_ms: u64,
    ) -> Result<DomainEvent, DomainError> {
        self.require_active()?;
        let name = name.into().trim().to_owned();
        if name.is_empty() {
            return Err(DomainError::InvalidField {
                field: "name",
                reason: "must not be empty".into(),
            });
        }
        self.name = name;
        self.touch(now_ms);
        Ok(self.updated_event())
    }

    /// Sets (or clears) the project's repository remote.
    ///
    /// An empty or whitespace-only string clears it, so a client can express
    /// "no remote" without a null-vs-absent distinction.
    pub fn set_git_remote_url(
        &mut self,
        url: Option<String>,
        now_ms: u64,
    ) -> Result<DomainEvent, DomainError> {
        self.require_active()?;
        self.git_remote_url = normalise_remote(url);
        self.touch(now_ms);
        Ok(self.updated_event())
    }

    /// Adds a source and returns the update event.
    ///
    /// A source must name a host and at least one of a path (an existing
    /// location) or a git remote (a declaration to clone later). The first
    /// source added becomes the project's default.
    pub fn add_source(
        &mut self,
        host_id: HostId,
        path: impl Into<String>,
        git_remote_url: Option<String>,
        now_ms: u64,
    ) -> Result<DomainEvent, DomainError> {
        self.require_active()?;
        let path = path.into().trim().to_owned();
        let git_remote_url = normalise_remote(git_remote_url);
        if path.is_empty() && git_remote_url.is_none() {
            return Err(DomainError::InvalidField {
                field: "path",
                reason: "a source needs a path or a git remote url".into(),
            });
        }
        let is_default = self.sources.is_empty();
        self.sources.push(ProjectSource {
            id: ProjectSourceId::mint(),
            project_id: self.id.clone(),
            host_id,
            path,
            git_remote_url,
            is_default,
            created_at_ms: now_ms,
            updated_at_ms: now_ms,
        });
        self.touch(now_ms);
        Ok(self.updated_event())
    }

    /// Removes a source, returning the update event or `None` when it is
    /// unknown.
    ///
    /// When the removed source was the default and others remain, the first
    /// remaining source becomes the new default, so a project with sources
    /// always has exactly one.
    pub fn remove_source(
        &mut self,
        source_id: &ProjectSourceId,
        now_ms: u64,
    ) -> Result<Option<DomainEvent>, DomainError> {
        self.require_active()?;
        let Some(index) = self
            .sources
            .iter()
            .position(|source| &source.id == source_id)
        else {
            return Ok(None);
        };
        let removed = self.sources.remove(index);
        if removed.is_default {
            if let Some(first) = self.sources.first_mut() {
                first.is_default = true;
                first.updated_at_ms = now_ms;
            }
        }
        self.touch(now_ms);
        Ok(Some(self.updated_event()))
    }

    /// Archives the project and returns the update event.
    ///
    /// Archiving is terminal for the record and refuses an already-archived
    /// project. It is **not** the domain's job to check for active threads:
    /// that requires the thread registry, so the server enforces it before
    /// calling this. See `docs/projects.md` for why a project with a run in
    /// flight cannot be archived, and why idle threads are not cascaded.
    pub fn archive(&mut self, now_ms: u64) -> Result<DomainEvent, DomainError> {
        self.require_active()?;
        self.archived_at_ms = Some(now_ms);
        self.touch(now_ms);
        Ok(self.updated_event())
    }

    fn require_active(&self) -> Result<(), DomainError> {
        if self.is_archived() {
            return Err(DomainError::Archived { entity: "project" });
        }
        Ok(())
    }

    fn touch(&mut self, now_ms: u64) {
        self.updated_at_ms = now_ms;
    }

    fn updated_event(&self) -> DomainEvent {
        DomainEvent::ProjectUpdated {
            project: self.clone(),
        }
    }
}

/// Trims a remote, collapsing an empty string to `None`.
fn normalise_remote(url: Option<String>) -> Option<String> {
    url.map(|url| url.trim().to_owned())
        .filter(|url| !url.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_rejects_an_empty_name() {
        assert!(matches!(
            Project::create("  ", ProjectKind::Personal, 1),
            Err(DomainError::InvalidField { field: "name", .. })
        ));
    }

    #[test]
    fn create_with_a_remote_announces_it_in_one_event() {
        let (project, event) =
            Project::create_with_remote("loom", ProjectKind::Standard, Some(" git@x ".into()), 1)
                .unwrap();
        assert_eq!(project.git_remote_url.as_deref(), Some("git@x"));
        assert!(matches!(event, DomainEvent::ProjectCreated { .. }));
    }

    #[test]
    fn a_blank_remote_is_normalised_away() {
        let (project, _) =
            Project::create_with_remote("loom", ProjectKind::Standard, Some("   ".into()), 1)
                .unwrap();
        assert_eq!(project.git_remote_url, None);
    }

    #[test]
    fn the_first_source_is_the_default() {
        let (mut project, _) = Project::create("loom", ProjectKind::Standard, 1).unwrap();
        project
            .add_source(HostId::mint(), "/srv/loom", None, 2)
            .unwrap();
        project
            .add_source(HostId::mint(), "/srv/other", None, 3)
            .unwrap();

        assert!(project.sources[0].is_default);
        assert!(!project.sources[1].is_default);
        assert_eq!(project.updated_at_ms, 3);
    }

    #[test]
    fn a_source_may_carry_a_git_remote() {
        let (mut project, _) = Project::create("loom", ProjectKind::Standard, 1).unwrap();
        project
            .add_source(HostId::mint(), "/srv/loom", Some("git@x:y/z".into()), 2)
            .unwrap();
        assert_eq!(
            project.sources[0].git_remote_url.as_deref(),
            Some("git@x:y/z")
        );
    }

    #[test]
    fn a_remote_only_source_is_allowed() {
        let (mut project, _) = Project::create("loom", ProjectKind::Standard, 1).unwrap();
        project
            .add_source(HostId::mint(), "", Some("git@x:y/z".into()), 2)
            .unwrap();
        assert!(project.sources[0].path.is_empty());
    }

    #[test]
    fn a_source_with_neither_path_nor_remote_is_rejected() {
        let (mut project, _) = Project::create("loom", ProjectKind::Standard, 1).unwrap();
        assert!(matches!(
            project.add_source(HostId::mint(), "  ", Some("  ".into()), 2),
            Err(DomainError::InvalidField { field: "path", .. })
        ));
        // A rejected add must not mutate the project.
        assert!(project.sources.is_empty());
    }

    #[test]
    fn removing_the_default_promotes_the_next_source() {
        let (mut project, _) = Project::create("loom", ProjectKind::Standard, 1).unwrap();
        project
            .add_source(HostId::mint(), "/srv/a", None, 2)
            .unwrap();
        project
            .add_source(HostId::mint(), "/srv/b", None, 3)
            .unwrap();
        let first = project.sources[0].id.clone();

        let event = project.remove_source(&first, 4).unwrap().unwrap();
        assert!(matches!(event, DomainEvent::ProjectUpdated { .. }));
        assert_eq!(project.sources.len(), 1);
        assert!(project.sources[0].is_default);
    }

    #[test]
    fn removing_an_unknown_source_is_a_noop() {
        let (mut project, _) = Project::create("loom", ProjectKind::Standard, 1).unwrap();
        assert!(project
            .remove_source(&ProjectSourceId::mint(), 2)
            .unwrap()
            .is_none());
    }

    #[test]
    fn renaming_rejects_an_empty_name_and_emits_an_update() {
        let (mut project, _) = Project::create("loom", ProjectKind::Standard, 1).unwrap();
        assert!(project.rename("  ", 2).is_err());
        assert_eq!(project.name, "loom");

        let event = project.rename(" loom-2 ", 3).unwrap();
        assert_eq!(project.name, "loom-2");
        assert!(matches!(event, DomainEvent::ProjectUpdated { .. }));
    }

    #[test]
    fn setting_and_clearing_the_remote_emits_an_update() {
        let (mut project, _) = Project::create("loom", ProjectKind::Standard, 1).unwrap();
        project.set_git_remote_url(Some("git@x".into()), 2).unwrap();
        assert_eq!(project.git_remote_url.as_deref(), Some("git@x"));

        project.set_git_remote_url(Some("   ".into()), 3).unwrap();
        assert_eq!(project.git_remote_url, None);
    }

    #[test]
    fn archiving_is_terminal_and_rejects_a_second_archive() {
        let (mut project, _) = Project::create("loom", ProjectKind::Standard, 1).unwrap();
        let event = project.archive(5).unwrap();
        assert!(project.is_archived());
        assert_eq!(project.archived_at_ms, Some(5));
        assert!(matches!(event, DomainEvent::ProjectUpdated { .. }));

        assert_eq!(
            project.archive(6),
            Err(DomainError::Archived { entity: "project" })
        );
    }

    #[test]
    fn an_archived_project_rejects_every_mutation() {
        let (mut project, _) = Project::create("loom", ProjectKind::Standard, 1).unwrap();
        project.archive(2).unwrap();

        let archived = Err(DomainError::Archived { entity: "project" });
        assert_eq!(project.rename("x", 3), archived.clone());
        assert_eq!(
            project.set_git_remote_url(Some("git@x".into()), 4),
            archived.clone()
        );
        assert_eq!(
            project.add_source(HostId::mint(), "/srv/x", None, 5),
            archived.clone()
        );
        assert!(matches!(
            project.remove_source(&ProjectSourceId::mint(), 6),
            Err(DomainError::Archived { entity: "project" })
        ));
    }

    #[test]
    fn adding_a_source_emits_a_project_update() {
        let (mut project, _) = Project::create("loom", ProjectKind::Standard, 1).unwrap();
        let event = project
            .add_source(HostId::mint(), "/srv/loom", None, 2)
            .unwrap();
        assert!(matches!(event, DomainEvent::ProjectUpdated { .. }));
    }
}
