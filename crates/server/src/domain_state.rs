//! In-memory domain state for the minimal command API.
//!
//! This is **not storage**. It is the smallest thing that makes the thread and
//! host commands meaningful in a single process: a registry to mint and
//! remember entities so a message can be rejected when its thread is unknown.
//! It is lost on restart, exactly like the default in-process relay backend,
//! and that is acceptable because persistence is a separate concern with its
//! own issue.
//!
//! Every mutation returns the [`DomainEvent`]s it produced. The handler
//! publishes them through [`crate::AppState::publish_domain_event`]; nothing
//! here touches the relay.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Mutex, MutexGuard};

use loom_domain::{
    DomainError, DomainEvent, Host, HostId, MessageRole, NewThread, Project, ProjectId,
    ProjectKind, Thread, ThreadId,
};

/// A command failed either because the target does not exist or because the
/// domain rejected the change.
#[derive(Debug)]
pub enum CommandError {
    /// The referenced entity is not known to this process.
    NotFound(String),
    /// The domain refused the change.
    Domain(DomainError),
}

impl fmt::Display for CommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CommandError::NotFound(message) => f.write_str(message),
            CommandError::Domain(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for CommandError {}

impl From<DomainError> for CommandError {
    fn from(error: DomainError) -> Self {
        CommandError::Domain(error)
    }
}

/// Projects, threads and hosts held in memory.
#[derive(Debug)]
pub struct DomainRegistry {
    inner: Mutex<RegistryInner>,
}

#[derive(Debug)]
struct RegistryInner {
    personal_project_id: ProjectId,
    projects: HashMap<ProjectId, Project>,
    threads: HashMap<ThreadId, Thread>,
    hosts: HashMap<HostId, Host>,
}

impl DomainRegistry {
    /// Creates the registry with the implicit personal project seeded.
    pub fn new(now_ms: u64) -> Self {
        let (project, _) = Project::create("Personal", ProjectKind::Personal, now_ms)
            .expect("a personal project always has a valid name");
        let personal_project_id = project.id.clone();
        let mut projects = HashMap::new();
        projects.insert(project.id.clone(), project);
        Self {
            inner: Mutex::new(RegistryInner {
                personal_project_id,
                projects,
                threads: HashMap::new(),
                hosts: HashMap::new(),
            }),
        }
    }

    /// The project a thread belongs to when the caller names none.
    pub fn personal_project_id(&self) -> ProjectId {
        self.lock().personal_project_id.clone()
    }

    /// Creates a thread and returns it with its creation event.
    pub fn create_thread(
        &self,
        project_id: Option<ProjectId>,
        title: Option<String>,
        now_ms: u64,
    ) -> Result<(Thread, DomainEvent), CommandError> {
        let mut inner = self.lock();
        let project_id = match project_id {
            Some(id) => {
                if !inner.projects.contains_key(&id) {
                    return Err(CommandError::NotFound(format!("project {id} is not known")));
                }
                id
            }
            None => inner.personal_project_id.clone(),
        };
        let (thread, event) = Thread::create(
            NewThread {
                project_id,
                title,
                parent_thread_id: None,
                environment_id: None,
            },
            now_ms,
        );
        inner.threads.insert(thread.id.clone(), thread.clone());
        Ok((thread, event))
    }

    /// Appends a message to a thread and returns the events it produced.
    pub fn post_message(
        &self,
        thread_id: &ThreadId,
        role: MessageRole,
        content: String,
        now_ms: u64,
    ) -> Result<Vec<DomainEvent>, CommandError> {
        let mut inner = self.lock();
        let thread = inner
            .threads
            .get_mut(thread_id)
            .ok_or_else(|| CommandError::NotFound(format!("thread {thread_id} is not known")))?;
        let events = thread.post_message(role, content, now_ms)?;
        Ok(events)
    }

    /// Registers a host and returns it with its registration event.
    pub fn register_host(
        &self,
        name: String,
        now_ms: u64,
    ) -> Result<(Host, DomainEvent), CommandError> {
        let mut inner = self.lock();
        let (host, event) = Host::register(name, now_ms)?;
        inner.hosts.insert(host.id.clone(), host.clone());
        Ok((host, event))
    }

    /// Looks up a thread.
    pub fn thread(&self, thread_id: &ThreadId) -> Option<Thread> {
        self.lock().threads.get(thread_id).cloned()
    }

    /// Looks up a host.
    pub fn host(&self, host_id: &HostId) -> Option<Host> {
        self.lock().hosts.get(host_id).cloned()
    }

    fn lock(&self) -> MutexGuard<'_, RegistryInner> {
        self.inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> DomainRegistry {
        DomainRegistry::new(1)
    }

    #[test]
    fn a_thread_defaults_to_the_personal_project() {
        let registry = registry();
        let (thread, _) = registry
            .create_thread(None, Some("first".into()), 2)
            .unwrap();
        assert_eq!(thread.project_id, registry.personal_project_id());
        assert_eq!(registry.thread(&thread.id).unwrap(), thread);
    }

    #[test]
    fn a_thread_cannot_be_created_in_an_unknown_project() {
        let registry = registry();
        assert!(matches!(
            registry.create_thread(Some(ProjectId::mint()), None, 2),
            Err(CommandError::NotFound(_))
        ));
    }

    #[test]
    fn messaging_an_unknown_thread_is_not_found() {
        let registry = registry();
        assert!(matches!(
            registry.post_message(&ThreadId::mint(), MessageRole::User, "hi".into(), 2),
            Err(CommandError::NotFound(_))
        ));
    }

    #[test]
    fn a_user_message_advances_the_stored_thread() {
        let registry = registry();
        let (thread, _) = registry.create_thread(None, None, 2).unwrap();
        let events = registry
            .post_message(&thread.id, MessageRole::User, "hello".into(), 3)
            .unwrap();

        assert_eq!(events.len(), 2);
        assert_eq!(
            registry.thread(&thread.id).unwrap().status,
            loom_domain::ThreadStatus::Working
        );
    }

    #[test]
    fn a_host_rejects_an_empty_name() {
        let registry = registry();
        assert!(matches!(
            registry.register_host("  ".into(), 2),
            Err(CommandError::Domain(DomainError::InvalidField {
                field: "name",
                ..
            }))
        ));
    }
}
