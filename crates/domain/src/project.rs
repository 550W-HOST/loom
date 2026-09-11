//! Projects and their sources.
//!
//! A project is the top-level container, usually one repository. Its
//! [`ProjectSource`]s say where the code lives: one project can map to paths on
//! several hosts, one source per host.

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
    /// The implicit project that owns work with no project of its own.
    Personal,
}

/// Where a project's code lives.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectSource {
    /// Identity.
    pub id: ProjectSourceId,
    /// The owning project.
    pub project_id: ProjectId,
    /// The enrolled host this path exists on.
    pub host_id: HostId,
    /// Absolute path on that host.
    pub path: String,
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
            git_remote_url: None,
            sources: Vec::new(),
            created_at_ms: now_ms,
            updated_at_ms: now_ms,
        };
        let event = DomainEvent::ProjectCreated {
            project: project.clone(),
        };
        Ok((project, event))
    }

    /// Adds a source and returns the update event.
    pub fn add_source(
        &mut self,
        host_id: HostId,
        path: impl Into<String>,
        now_ms: u64,
    ) -> Result<DomainEvent, DomainError> {
        let path = path.into().trim().to_owned();
        if path.is_empty() {
            return Err(DomainError::InvalidField {
                field: "path",
                reason: "must not be empty".into(),
            });
        }
        let is_default = self.sources.is_empty();
        self.sources.push(ProjectSource {
            id: ProjectSourceId::mint(),
            project_id: self.id.clone(),
            host_id,
            path,
            is_default,
            created_at_ms: now_ms,
            updated_at_ms: now_ms,
        });
        self.updated_at_ms = now_ms;
        Ok(DomainEvent::ProjectUpdated {
            project: self.clone(),
        })
    }
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
    fn the_first_source_is_the_default() {
        let (mut project, _) = Project::create("loom", ProjectKind::Standard, 1).unwrap();
        project.add_source(HostId::mint(), "/srv/loom", 2).unwrap();
        project.add_source(HostId::mint(), "/srv/other", 3).unwrap();

        assert!(project.sources[0].is_default);
        assert!(!project.sources[1].is_default);
        assert_eq!(project.updated_at_ms, 3);
    }

    #[test]
    fn adding_a_source_emits_a_project_update() {
        let (mut project, _) = Project::create("loom", ProjectKind::Standard, 1).unwrap();
        let event = project.add_source(HostId::mint(), "/srv/loom", 2).unwrap();
        assert!(matches!(event, DomainEvent::ProjectUpdated { .. }));
    }
}
