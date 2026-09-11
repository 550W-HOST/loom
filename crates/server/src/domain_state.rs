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

    /// Enrolls a daemon as a host, idempotently.
    ///
    /// This is the operation a daemon performs when it connects, and it is
    /// why a daemon's identity survives reconnects:
    ///
    /// * `host_id: Some(id)` and the host is known — it is marked connected
    ///   and, if it was disconnected, a `host_status_changed` event is
    ///   produced. No second machine is created.
    /// * `host_id: Some(id)` and the host is unknown — it is created under the
    ///   identity the daemon supplied, which is how a server started after the
    ///   daemon still recognises it.
    /// * `host_id: None` — a fresh identity is minted, for a daemon that has
    ///   never enrolled before.
    pub fn enroll_host(
        &self,
        host_id: Option<HostId>,
        name: String,
        now_ms: u64,
    ) -> Result<(Host, Vec<DomainEvent>), CommandError> {
        let mut inner = self.lock();
        if let Some(id) = host_id {
            if let Some(existing) = inner.hosts.get_mut(&id) {
                let events = existing.mark_connected(now_ms).into_iter().collect();
                return Ok((existing.clone(), events));
            }
            let (host, event) = Host::register_as(Some(id), name, now_ms)?;
            inner.hosts.insert(host.id.clone(), host.clone());
            return Ok((host, vec![event]));
        }
        let (host, event) = Host::register(name, now_ms)?;
        inner.hosts.insert(host.id.clone(), host.clone());
        Ok((host, vec![event]))
    }

    /// Every known host, in id order.
    pub fn hosts(&self) -> Vec<Host> {
        let mut hosts: Vec<Host> = self.lock().hosts.values().cloned().collect();
        hosts.sort_by(|left, right| left.id.cmp(&right.id));
        hosts
    }

    /// Records a heartbeat without publishing a frame.
    pub fn host_heartbeat(&self, host_id: &HostId, now_ms: u64) -> Result<Host, CommandError> {
        let mut inner = self.lock();
        let host = inner
            .hosts
            .get_mut(host_id)
            .ok_or_else(|| CommandError::NotFound(format!("host {host_id} is not known")))?;
        host.heartbeat(now_ms);
        Ok(host.clone())
    }

    /// Marks a host's daemon detached, returning an event on an actual change.
    pub fn mark_host_disconnected(
        &self,
        host_id: &HostId,
        now_ms: u64,
    ) -> Result<Vec<DomainEvent>, CommandError> {
        let mut inner = self.lock();
        let host = inner
            .hosts
            .get_mut(host_id)
            .ok_or_else(|| CommandError::NotFound(format!("host {host_id} is not known")))?;
        Ok(host.mark_disconnected(now_ms).into_iter().collect())
    }

    /// The host primary-host queries should use.
    ///
    /// `local_host_id` is the machine the *server* runs on, if the operator
    /// declared one. It is a preference, never a requirement: with no local
    /// daemon the answer falls back to a connected remote host, and with no
    /// host at all it is `None`. See
    /// [`loom_domain::select_primary_host`].
    pub fn primary_host(&self, local_host_id: Option<&HostId>) -> Option<Host> {
        let hosts: Vec<Host> = self.lock().hosts.values().cloned().collect();
        loom_domain::select_primary_host(&hosts, local_host_id).cloned()
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

    #[test]
    fn enrolling_the_same_host_twice_keeps_one_machine() {
        let registry = registry();
        let (host, events) = registry.enroll_host(None, "laptop".into(), 2).unwrap();
        assert_eq!(events.len(), 1);

        // The daemon reconnects with the id it was given: no second host, no
        // registration event, and the status was already connected.
        let (again, events) = registry
            .enroll_host(Some(host.id.clone()), "laptop".into(), 3)
            .unwrap();
        assert_eq!(again.id, host.id);
        assert!(events.is_empty());
        assert_eq!(registry.hosts().len(), 1);
    }

    #[test]
    fn a_reconnect_after_a_disconnect_emits_a_status_change() {
        let registry = registry();
        let (host, _) = registry.enroll_host(None, "laptop".into(), 2).unwrap();
        registry.mark_host_disconnected(&host.id, 3).unwrap();
        assert_eq!(
            registry.host(&host.id).unwrap().status,
            loom_domain::HostStatus::Disconnected
        );

        let (_, events) = registry
            .enroll_host(Some(host.id.clone()), "laptop".into(), 4)
            .unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(
            registry.host(&host.id).unwrap().status,
            loom_domain::HostStatus::Connected
        );
    }

    #[test]
    fn a_server_with_no_daemon_has_no_primary_host_but_no_error() {
        let registry = registry();
        assert!(registry.primary_host(Some(&HostId::mint())).is_none());
    }

    #[test]
    fn the_primary_falls_back_to_a_remote_host_when_the_local_one_is_absent() {
        let registry = registry();
        let (remote, _) = registry.enroll_host(None, "remote".into(), 2).unwrap();
        let absent_local = HostId::mint();

        let primary = registry.primary_host(Some(&absent_local)).unwrap();
        assert_eq!(primary.id, remote.id);
    }

    #[test]
    fn heartbeating_an_unknown_host_is_not_found() {
        let registry = registry();
        assert!(matches!(
            registry.host_heartbeat(&HostId::mint(), 2),
            Err(CommandError::NotFound(_))
        ));
    }
}
