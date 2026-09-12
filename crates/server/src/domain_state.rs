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
    DomainError, DomainEvent, Environment, EnvironmentId, EnvironmentKind, EnvironmentStatus, Host,
    HostId, MessageRole, NewThread, Project, ProjectId, ProjectKind, ProjectSourceId, RunId,
    Thread, ThreadId, ThreadStatus, ThreadTrigger,
};
use serde::{Deserialize, Serialize};

/// A command failed either because the target does not exist or because the
/// domain rejected the change.
#[derive(Debug, PartialEq, Eq)]
pub enum CommandError {
    /// The referenced entity is not known to this process.
    NotFound(String),
    /// The change is well-formed but conflicts with the current state.
    ///
    /// Distinct from [`CommandError::Domain`] because the conflict is a
    /// registry-level rule the domain type cannot see — archiving a project
    /// that still has a run in flight, for example.
    Conflict(String),
    /// The domain refused the change.
    Domain(DomainError),
}

impl fmt::Display for CommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CommandError::NotFound(message) => f.write_str(message),
            CommandError::Conflict(message) => f.write_str(message),
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

/// A point-in-time copy of the registry, as stored in a domain snapshot.
///
/// Vectors rather than maps so the serialized form is stable and diffable;
/// [`DomainRegistry::export`] sorts each one by id before returning.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistrySnapshot {
    /// The workspace's seeded project.
    pub personal_project_id: ProjectId,
    /// Every project.
    pub projects: Vec<Project>,
    /// Every thread.
    pub threads: Vec<Thread>,
    /// Every host.
    pub hosts: Vec<Host>,
    /// Every environment.
    pub environments: Vec<Environment>,
}

#[derive(Debug)]
struct RegistryInner {
    personal_project_id: ProjectId,
    projects: HashMap<ProjectId, Project>,
    threads: HashMap<ThreadId, Thread>,
    hosts: HashMap<HostId, Host>,
    environments: HashMap<EnvironmentId, Environment>,
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
                environments: HashMap::new(),
            }),
        }
    }

    /// The id of the workspace's seeded project.
    ///
    /// It is returned so a client can select it, never as a fallback for a
    /// missing project: threads and environments must name their project
    /// explicitly.
    pub fn personal_project_id(&self) -> ProjectId {
        self.lock().personal_project_id.clone()
    }

    /// Creates a project and returns it with its creation event.
    ///
    /// `kind` is [`ProjectKind::Standard`] for every project a client creates.
    /// The seeded project's [`ProjectKind::Personal`] is provenance, not
    /// privilege, so this accepts it too rather than special-casing it.
    pub fn create_project(
        &self,
        name: String,
        kind: ProjectKind,
        git_remote_url: Option<String>,
        now_ms: u64,
    ) -> Result<(Project, DomainEvent), CommandError> {
        let mut inner = self.lock();
        let (project, event) = Project::create_with_remote(name, kind, git_remote_url, now_ms)?;
        inner.projects.insert(project.id.clone(), project.clone());
        Ok((project, event))
    }

    /// Looks up a project.
    pub fn project(&self, project_id: &ProjectId) -> Option<Project> {
        self.lock().projects.get(project_id).cloned()
    }

    /// Every known project, in a stable order.
    ///
    /// Sorted by creation time and then id, so the list is deterministic and
    /// does not depend on `HashMap` iteration order. Active projects come
    /// first; archived ones follow, still sorted the same way.
    pub fn projects(&self) -> Vec<Project> {
        let mut projects: Vec<Project> = self.lock().projects.values().cloned().collect();
        projects.sort_by(|left, right| {
            left.is_archived()
                .cmp(&right.is_archived())
                .then_with(|| left.created_at_ms.cmp(&right.created_at_ms))
                .then_with(|| left.id.cmp(&right.id))
        });
        projects
    }

    /// Renames a project and returns the update event.
    pub fn rename_project(
        &self,
        project_id: &ProjectId,
        name: String,
        now_ms: u64,
    ) -> Result<(Project, DomainEvent), CommandError> {
        let mut inner = self.lock();
        let project = inner
            .projects
            .get_mut(project_id)
            .ok_or_else(|| CommandError::NotFound(format!("project {project_id} is not known")))?;
        let event = project.rename(name, now_ms)?;
        Ok((project.clone(), event))
    }

    /// Changes a project's repository remote and returns the update event.
    ///
    /// An empty or whitespace-only string clears the remote. The domain's
    /// [`Project::set_git_remote_url`] normalises it, so a client can send `""`
    /// rather than a null.
    pub fn set_project_git_remote(
        &self,
        project_id: &ProjectId,
        url: Option<String>,
        now_ms: u64,
    ) -> Result<(Project, DomainEvent), CommandError> {
        let mut inner = self.lock();
        let project = inner
            .projects
            .get_mut(project_id)
            .ok_or_else(|| CommandError::NotFound(format!("project {project_id} is not known")))?;
        let event = project.set_git_remote_url(url, now_ms)?;
        Ok((project.clone(), event))
    }

    /// Adds a source to a project and returns the update event.
    pub fn add_project_source(
        &self,
        project_id: &ProjectId,
        host_id: HostId,
        path: String,
        git_remote_url: Option<String>,
        now_ms: u64,
    ) -> Result<(Project, DomainEvent), CommandError> {
        let mut inner = self.lock();
        if !inner.hosts.contains_key(&host_id) {
            return Err(CommandError::NotFound(format!(
                "host {host_id} is not known"
            )));
        }
        let project = inner
            .projects
            .get_mut(project_id)
            .ok_or_else(|| CommandError::NotFound(format!("project {project_id} is not known")))?;
        let event = project.add_source(host_id, path, git_remote_url, now_ms)?;
        Ok((project.clone(), event))
    }

    /// Removes a source from a project, returning the update event or `None`
    /// when the source is unknown.
    pub fn remove_project_source(
        &self,
        project_id: &ProjectId,
        source_id: &ProjectSourceId,
        now_ms: u64,
    ) -> Result<Option<(Project, DomainEvent)>, CommandError> {
        let mut inner = self.lock();
        let project = inner
            .projects
            .get_mut(project_id)
            .ok_or_else(|| CommandError::NotFound(format!("project {project_id} is not known")))?;
        match project.remove_source(source_id, now_ms)? {
            Some(event) => Ok(Some((project.clone(), event))),
            None => Ok(None),
        }
    }

    /// Archives a project, refusing one that still has a run in flight.
    ///
    /// The rule is **refuse, never cascade**: archiving a project whose threads
    /// are `working` or `waiting` is a conflict, because those threads have a
    /// provider running against a workspace inside the project. Idle, errored
    /// and archived threads do not block the archive and are left untouched —
    /// they keep their project reference. See `docs/projects.md`.
    pub fn archive_project(
        &self,
        project_id: &ProjectId,
        now_ms: u64,
    ) -> Result<(Project, DomainEvent), CommandError> {
        let mut inner = self.lock();
        let busy = inner
            .threads
            .values()
            .filter(|thread| &thread.project_id == project_id)
            .filter(|thread| matches!(thread.status, ThreadStatus::Working | ThreadStatus::Waiting))
            .count();
        let project = inner
            .projects
            .get_mut(project_id)
            .ok_or_else(|| CommandError::NotFound(format!("project {project_id} is not known")))?;
        // Checked even when the project is already archived, so a second
        // archive reports the lifecycle error rather than a stale busy count.
        if !project.is_archived() && busy > 0 {
            return Err(CommandError::Conflict(format!(
                "project {project_id} has {busy} thread(s) with a run in flight; archive them first"
            )));
        }
        let event = project.archive(now_ms)?;
        Ok((project.clone(), event))
    }

    /// Creates a thread and returns it with its creation event.
    ///
    /// The project is **required**: `None` is rejected rather than silently
    /// landing in the seeded personal project. A thread's project is the
    /// container it is listed under, and guessing it is how every thread ended
    /// up in one implicit project. An unknown project, an archived project and
    /// an unknown environment are all rejected here rather than at dispatch.
    pub fn create_thread(
        &self,
        project_id: Option<ProjectId>,
        title: Option<String>,
        environment_id: Option<EnvironmentId>,
        now_ms: u64,
    ) -> Result<(Thread, DomainEvent), CommandError> {
        let mut inner = self.lock();
        let Some(project_id) = project_id else {
            return Err(CommandError::Domain(DomainError::InvalidField {
                field: "project_id",
                reason: "a thread must belong to a project".into(),
            }));
        };
        let Some(project) = inner.projects.get(&project_id) else {
            return Err(CommandError::NotFound(format!(
                "project {project_id} is not known"
            )));
        };
        if project.is_archived() {
            return Err(CommandError::Conflict(format!(
                "project {project_id} is archived; unarchive it before creating a thread"
            )));
        }
        if let Some(id) = &environment_id {
            if !inner.environments.contains_key(id) {
                return Err(CommandError::NotFound(format!(
                    "environment {id} is not known"
                )));
            }
        }
        let (thread, event) = Thread::create(
            NewThread {
                project_id,
                title,
                parent_thread_id: None,
                environment_id,
            },
            now_ms,
        );
        inner.threads.insert(thread.id.clone(), thread.clone());
        Ok((thread, event))
    }

    /// Creates an environment and returns it with the events it produced.
    ///
    /// The owning project is **required**, exactly like a thread: an
    /// environment's workspace belongs to a project. An unknown or archived
    /// project is rejected, and so is the personal default this method no
    /// longer guesses.
    ///
    /// An `unmanaged` environment starts `ready` with its path; a `managed`
    /// one starts `creating` with none, and is provisioned later by a daemon.
    pub fn create_environment(
        &self,
        project_id: Option<ProjectId>,
        host_id: HostId,
        kind: EnvironmentKind,
        path: Option<String>,
        now_ms: u64,
    ) -> Result<(Environment, Vec<DomainEvent>), CommandError> {
        let mut inner = self.lock();
        let Some(project_id) = project_id else {
            return Err(CommandError::Domain(DomainError::InvalidField {
                field: "project_id",
                reason: "an environment must belong to a project".into(),
            }));
        };
        let Some(project) = inner.projects.get(&project_id) else {
            return Err(CommandError::NotFound(format!(
                "project {project_id} is not known"
            )));
        };
        if project.is_archived() {
            return Err(CommandError::Conflict(format!(
                "project {project_id} is archived; unarchive it before creating an environment"
            )));
        }
        if !inner.hosts.contains_key(&host_id) {
            return Err(CommandError::NotFound(format!(
                "host {host_id} is not known"
            )));
        }
        let (environment, event) = Environment::create(project_id, host_id, kind, path, now_ms)?;
        inner
            .environments
            .insert(environment.id.clone(), environment.clone());
        Ok((environment, vec![event]))
    }

    /// Looks up an environment.
    pub fn environment(&self, environment_id: &EnvironmentId) -> Option<Environment> {
        self.lock().environments.get(environment_id).cloned()
    }

    /// Every known environment, oldest first.
    pub fn environments(&self) -> Vec<Environment> {
        let mut environments: Vec<Environment> =
            self.lock().environments.values().cloned().collect();
        environments.sort_by(|left, right| left.id.cmp(&right.id));
        environments
    }

    /// Every environment owned by a project, oldest first.
    pub fn environments_for_project(&self, project_id: &ProjectId) -> Vec<Environment> {
        let mut environments: Vec<Environment> = self
            .lock()
            .environments
            .values()
            .filter(|environment| &environment.project_id == project_id)
            .cloned()
            .collect();
        environments.sort_by(|left, right| left.id.cmp(&right.id));
        environments
    }

    /// Moves an environment to `to` and returns the status-change event.
    ///
    /// An illegal transition is a `CommandError::Domain`, not a silent no-op:
    /// the caller decides whether a late report is harmless.
    pub fn set_environment_status(
        &self,
        environment_id: &EnvironmentId,
        to: EnvironmentStatus,
        now_ms: u64,
    ) -> Result<(Environment, DomainEvent), CommandError> {
        let mut inner = self.lock();
        let environment = inner.environments.get_mut(environment_id).ok_or_else(|| {
            CommandError::NotFound(format!("environment {environment_id} is not known"))
        })?;
        let event = environment.set_status(to, now_ms)?;
        Ok((environment.clone(), event))
    }

    /// Moves an environment to `error`, recording why.
    pub fn fail_environment(
        &self,
        environment_id: &EnvironmentId,
        reason: String,
        now_ms: u64,
    ) -> Result<(Environment, DomainEvent), CommandError> {
        let mut inner = self.lock();
        let environment = inner.environments.get_mut(environment_id).ok_or_else(|| {
            CommandError::NotFound(format!("environment {environment_id} is not known"))
        })?;
        let event = environment.set_error(reason, now_ms)?;
        Ok((environment.clone(), event))
    }

    /// Records the path a managed environment was provisioned at.
    pub fn set_environment_path(
        &self,
        environment_id: &EnvironmentId,
        path: String,
        now_ms: u64,
    ) -> Result<Environment, CommandError> {
        let mut inner = self.lock();
        let environment = inner.environments.get_mut(environment_id).ok_or_else(|| {
            CommandError::NotFound(format!("environment {environment_id} is not known"))
        })?;
        environment.set_provisioned_path(path, now_ms)?;
        Ok(environment.clone())
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

    /// Every known thread, newest first.
    ///
    /// The list a UI renders in its sidebar. Ordering is by creation time and
    /// then id, so it is stable when two threads share a millisecond.
    pub fn threads(&self) -> Vec<Thread> {
        let mut threads: Vec<Thread> = self.lock().threads.values().cloned().collect();
        threads.sort_by(|left, right| {
            right
                .created_at_ms
                .cmp(&left.created_at_ms)
                .then_with(|| left.id.cmp(&right.id))
        });
        threads
    }

    /// Applies a lifecycle trigger to a stored thread.
    ///
    /// Returns the status-change event when the transition happened, `None`
    /// when the thread is unknown or already in a status the trigger does not
    /// apply to. The `None` case is what makes reconciliation idempotent: a
    /// run reaped twice cannot flip a thread twice.
    pub fn transition_thread(
        &self,
        thread_id: &ThreadId,
        trigger: ThreadTrigger,
        now_ms: u64,
    ) -> Result<Option<DomainEvent>, CommandError> {
        let mut inner = self.lock();
        let thread = inner
            .threads
            .get_mut(thread_id)
            .ok_or_else(|| CommandError::NotFound(format!("thread {thread_id} is not known")))?;
        match thread.transition(trigger, now_ms) {
            Ok(event) => Ok(Some(event)),
            // The thread is not in a status the trigger applies to: not an
            // error for a reconciler that races a report.
            Err(DomainError::IllegalThreadTransition { .. }) => Ok(None),
            Err(error) => Err(CommandError::Domain(error)),
        }
    }

    /// Records the run now advancing a thread.
    pub fn set_thread_run(
        &self,
        thread_id: &ThreadId,
        run_id: &RunId,
        now_ms: u64,
    ) -> Result<(), CommandError> {
        let mut inner = self.lock();
        let thread = inner
            .threads
            .get_mut(thread_id)
            .ok_or_else(|| CommandError::NotFound(format!("thread {thread_id} is not known")))?;
        thread.begin_run(run_id.clone(), now_ms);
        Ok(())
    }

    /// Clears a thread's recorded run once it has ended.
    pub fn clear_thread_run(&self, thread_id: &ThreadId, now_ms: u64) -> Result<(), CommandError> {
        let mut inner = self.lock();
        let thread = inner
            .threads
            .get_mut(thread_id)
            .ok_or_else(|| CommandError::NotFound(format!("thread {thread_id} is not known")))?;
        thread.clear_run(now_ms);
        Ok(())
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

    /// Copies every entity out, in id order, for a durable snapshot.
    ///
    /// This is the entity view, not the log: messages and run history are
    /// deliberately absent, because those are what the relay log is for.
    pub fn export(&self) -> RegistrySnapshot {
        let inner = self.lock();
        let mut projects: Vec<Project> = inner.projects.values().cloned().collect();
        projects.sort_by(|left, right| left.id.cmp(&right.id));
        let mut threads: Vec<Thread> = inner.threads.values().cloned().collect();
        threads.sort_by(|left, right| left.id.cmp(&right.id));
        let mut hosts: Vec<Host> = inner.hosts.values().cloned().collect();
        hosts.sort_by(|left, right| left.id.cmp(&right.id));
        let mut environments: Vec<Environment> = inner.environments.values().cloned().collect();
        environments.sort_by(|left, right| left.id.cmp(&right.id));
        RegistrySnapshot {
            personal_project_id: inner.personal_project_id.clone(),
            projects,
            threads,
            hosts,
            environments,
        }
    }

    /// Replaces every entity with a snapshot's contents.
    ///
    /// The caller replays the log "after" the snapshot's watermark once this
    /// returns; this restores only the baseline. The personal project is kept
    /// even if the snapshot omitted it, so `create_thread` always resolves.
    pub fn restore(&self, snapshot: RegistrySnapshot) {
        let mut inner = self.lock();
        *inner = RegistryInner::from_snapshot(snapshot);
    }

    /// Applies one already-published domain event to the registry.
    ///
    /// This is replay, not command handling. It is idempotent, never fails and
    /// never panics: an event whose entity is unknown, or one that does not
    /// affect the entity view (a message), is a no-op. That tolerance is what
    /// lets recovery run against a log that is older, newer or more complete
    /// than the snapshot without a correctness cliff.
    pub fn apply_event(&self, event: &DomainEvent) {
        let mut inner = self.lock();
        match event {
            DomainEvent::ProjectCreated { project } | DomainEvent::ProjectUpdated { project } => {
                inner.projects.insert(project.id.clone(), project.clone());
            }
            DomainEvent::ThreadCreated { thread } => {
                inner.threads.insert(thread.id.clone(), thread.clone());
            }
            DomainEvent::ThreadStatusChanged {
                thread_id,
                to,
                at_ms,
                ..
            } => {
                if let Some(thread) = inner.threads.get_mut(thread_id) {
                    thread.status = *to;
                    thread.updated_at_ms = *at_ms;
                    thread.archived_at_ms = (*to == ThreadStatus::Archived).then_some(*at_ms);
                }
            }
            // Messages are the log's business; the registry holds no timeline.
            DomainEvent::ThreadMessageAdded { .. } => {}
            DomainEvent::ThreadRunEvent { run } => {
                if let Some(thread) = inner.threads.get_mut(&run.thread_id) {
                    thread.active_run_id = if run.event.is_terminal() {
                        None
                    } else {
                        Some(run.run_id.clone())
                    };
                    thread.updated_at_ms = run.at_ms;
                }
            }
            DomainEvent::HostRegistered { host } => {
                inner.hosts.insert(host.id.clone(), host.clone());
            }
            DomainEvent::HostStatusChanged {
                host_id, to, at_ms, ..
            } => {
                if let Some(host) = inner.hosts.get_mut(host_id) {
                    host.status = *to;
                    host.updated_at_ms = *at_ms;
                }
            }
            DomainEvent::EnvironmentCreated { environment } => {
                inner
                    .environments
                    .insert(environment.id.clone(), environment.clone());
            }
            DomainEvent::EnvironmentStatusChanged {
                environment_id,
                to,
                at_ms,
                ..
            } => {
                if let Some(environment) = inner.environments.get_mut(environment_id) {
                    environment.status = *to;
                    environment.updated_at_ms = *at_ms;
                    if *to != EnvironmentStatus::Error {
                        environment.error = None;
                    }
                }
            }
        }
    }
}

impl RegistryInner {
    fn from_snapshot(snapshot: RegistrySnapshot) -> Self {
        let mut projects: HashMap<ProjectId, Project> = snapshot
            .projects
            .into_iter()
            .map(|project| (project.id.clone(), project))
            .collect();
        let threads = snapshot
            .threads
            .into_iter()
            .map(|thread| (thread.id.clone(), thread))
            .collect();
        let hosts = snapshot
            .hosts
            .into_iter()
            .map(|host| (host.id.clone(), host))
            .collect();
        let environments = snapshot
            .environments
            .into_iter()
            .map(|environment| (environment.id.clone(), environment))
            .collect();
        let personal_project_id = snapshot.personal_project_id;
        // Defensive: a snapshot that lost its personal project still has to
        // resolve a default, so the id stays stable and a placeholder is
        // synthesised rather than minting a second identity.
        projects
            .entry(personal_project_id.clone())
            .or_insert(Project {
                id: personal_project_id.clone(),
                kind: ProjectKind::Personal,
                name: "Personal".into(),
                git_remote_url: None,
                sources: Vec::new(),
                archived_at_ms: None,
                created_at_ms: 0,
                updated_at_ms: 0,
            });
        Self {
            personal_project_id,
            projects,
            threads,
            hosts,
            environments,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> DomainRegistry {
        DomainRegistry::new(1)
    }

    /// The seeded project's id, the explicit owner every command must name.
    fn personal(registry: &DomainRegistry) -> ProjectId {
        registry.personal_project_id()
    }

    #[test]
    fn a_thread_must_name_a_project_and_the_seeded_one_works() {
        let registry = registry();
        let (thread, _) = registry
            .create_thread(Some(personal(&registry)), Some("first".into()), None, 2)
            .unwrap();
        assert_eq!(thread.project_id, personal(&registry));
        assert_eq!(registry.thread(&thread.id).unwrap(), thread);
    }

    #[test]
    fn a_thread_with_no_project_is_rejected() {
        let registry = registry();
        assert!(matches!(
            registry.create_thread(None, Some("first".into()), None, 2),
            Err(CommandError::Domain(DomainError::InvalidField {
                field: "project_id",
                ..
            }))
        ));
    }

    #[test]
    fn a_thread_cannot_be_created_in_an_unknown_project() {
        let registry = registry();
        assert!(matches!(
            registry.create_thread(Some(ProjectId::mint()), None, None, 2),
            Err(CommandError::NotFound(_))
        ));
    }

    #[test]
    fn a_thread_cannot_be_created_in_an_archived_project() {
        let registry = registry();
        let project = personal(&registry);
        registry.archive_project(&project, 2).unwrap();
        assert!(matches!(
            registry.create_thread(Some(project), None, None, 3),
            Err(CommandError::Conflict(_))
        ));
    }

    #[test]
    fn a_thread_cannot_be_bound_to_an_unknown_environment() {
        let registry = registry();
        assert!(matches!(
            registry.create_thread(
                Some(personal(&registry)),
                None,
                Some(EnvironmentId::mint()),
                2
            ),
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
        let (thread, _) = registry
            .create_thread(Some(personal(&registry)), None, None, 2)
            .unwrap();
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

    fn enrolled(registry: &DomainRegistry) -> HostId {
        registry.enroll_host(None, "laptop".into(), 2).unwrap().0.id
    }

    #[test]
    fn an_unmanaged_environment_starts_ready_with_its_path() {
        let registry = registry();
        let host = enrolled(&registry);
        let (environment, events) = registry
            .create_environment(
                Some(personal(&registry)),
                host,
                EnvironmentKind::Unmanaged,
                Some("/srv/loom".into()),
                3,
            )
            .unwrap();

        assert_eq!(environment.status, EnvironmentStatus::Ready);
        assert_eq!(environment.path.as_deref(), Some("/srv/loom"));
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], DomainEvent::EnvironmentCreated { .. }));
        assert_eq!(
            registry.environment(&environment.id),
            Some(environment.clone())
        );
        assert_eq!(
            registry.environments_for_project(&personal(&registry)),
            vec![environment]
        );
    }

    #[test]
    fn a_managed_environment_starts_creating_and_advances_through_provisioning() {
        let registry = registry();
        let host = enrolled(&registry);
        let (environment, _) = registry
            .create_environment(
                Some(personal(&registry)),
                host,
                EnvironmentKind::Managed,
                None,
                3,
            )
            .unwrap();
        assert_eq!(environment.status, EnvironmentStatus::Creating);
        assert_eq!(environment.path, None);

        let (provisioning, event) = registry
            .set_environment_status(&environment.id, EnvironmentStatus::Provisioning, 4)
            .unwrap();
        assert_eq!(provisioning.status, EnvironmentStatus::Provisioning);
        assert!(matches!(
            event,
            DomainEvent::EnvironmentStatusChanged { .. }
        ));

        registry
            .set_environment_path(&environment.id, "/srv/work".into(), 5)
            .unwrap();
        let (ready, _) = registry
            .set_environment_status(&environment.id, EnvironmentStatus::Ready, 6)
            .unwrap();
        assert_eq!(ready.status, EnvironmentStatus::Ready);
        assert_eq!(ready.path.as_deref(), Some("/srv/work"));
    }

    #[test]
    fn a_failed_provision_is_recorded_as_error_with_its_reason() {
        let registry = registry();
        let host = enrolled(&registry);
        let (environment, _) = registry
            .create_environment(
                Some(personal(&registry)),
                host,
                EnvironmentKind::Managed,
                None,
                3,
            )
            .unwrap();
        registry
            .set_environment_status(&environment.id, EnvironmentStatus::Provisioning, 4)
            .unwrap();
        let (failed, event) = registry
            .fail_environment(&environment.id, "permission denied".into(), 5)
            .unwrap();
        assert_eq!(failed.status, EnvironmentStatus::Error);
        assert_eq!(failed.error.as_deref(), Some("permission denied"));
        assert!(matches!(
            event,
            DomainEvent::EnvironmentStatusChanged { .. }
        ));
    }

    #[test]
    fn destroying_an_environment_is_terminal() {
        let registry = registry();
        let host = enrolled(&registry);
        let (environment, _) = registry
            .create_environment(
                Some(personal(&registry)),
                host,
                EnvironmentKind::Unmanaged,
                Some("/srv/loom".into()),
                3,
            )
            .unwrap();
        registry
            .set_environment_status(&environment.id, EnvironmentStatus::Destroyed, 4)
            .unwrap();
        assert!(matches!(
            registry.set_environment_status(&environment.id, EnvironmentStatus::Ready, 5),
            Err(CommandError::Domain(
                DomainError::IllegalEnvironmentTransition { .. }
            ))
        ));
    }

    #[test]
    fn an_environment_in_an_unknown_project_or_host_is_not_found() {
        let registry = registry();
        let host = enrolled(&registry);
        assert!(matches!(
            registry.create_environment(
                Some(ProjectId::mint()),
                host,
                EnvironmentKind::Unmanaged,
                Some("/srv/loom".into()),
                3,
            ),
            Err(CommandError::NotFound(_))
        ));
        assert!(matches!(
            registry.create_environment(
                None,
                HostId::mint(),
                EnvironmentKind::Unmanaged,
                Some("/srv/loom".into()),
                3,
            ),
            Err(CommandError::Domain(DomainError::InvalidField {
                field: "project_id",
                ..
            }))
        ));
        assert!(matches!(
            registry.create_environment(
                Some(personal(&registry)),
                HostId::mint(),
                EnvironmentKind::Unmanaged,
                Some("/srv/loom".into()),
                3,
            ),
            Err(CommandError::NotFound(_))
        ));
    }

    #[test]
    fn a_project_is_created_listed_and_renamed() {
        let registry = registry();
        let (project, event) = registry
            .create_project(
                "loom".into(),
                ProjectKind::Standard,
                Some("git@x:y/loom".into()),
                2,
            )
            .unwrap();
        assert!(matches!(event, DomainEvent::ProjectCreated { .. }));
        assert_eq!(project.git_remote_url.as_deref(), Some("git@x:y/loom"));
        assert_eq!(registry.project(&project.id), Some(project.clone()));

        let (renamed, event) = registry
            .rename_project(&project.id, "Loom".into(), 3)
            .unwrap();
        assert_eq!(renamed.name, "Loom");
        assert!(matches!(event, DomainEvent::ProjectUpdated { .. }));
        assert!(matches!(
            registry.rename_project(&ProjectId::mint(), "x".into(), 4),
            Err(CommandError::NotFound(_))
        ));
    }

    #[test]
    fn a_project_supports_multiple_sources_on_different_hosts() {
        let registry = registry();
        let (first, _) = registry.enroll_host(None, "laptop".into(), 2).unwrap();
        let (second, _) = registry.enroll_host(None, "desktop".into(), 2).unwrap();
        let (project, _) = registry
            .create_project("loom".into(), ProjectKind::Standard, None, 2)
            .unwrap();

        let (project, _) = registry
            .add_project_source(&project.id, first.id.clone(), "/srv/loom".into(), None, 3)
            .unwrap();
        let (project, _) = registry
            .add_project_source(
                &project.id,
                second.id.clone(),
                "/home/me/loom".into(),
                Some("git@x:y/loom".into()),
                4,
            )
            .unwrap();

        assert_eq!(project.sources.len(), 2);
        assert!(project.sources[0].is_default);
        assert!(!project.sources[1].is_default);
        assert_eq!(project.sources[1].host_id, second.id);
        assert_eq!(
            project.sources[1].git_remote_url.as_deref(),
            Some("git@x:y/loom")
        );

        // A source on an unknown host is rejected before anything is added.
        assert!(matches!(
            registry.add_project_source(&project.id, HostId::mint(), "/srv/x".into(), None, 5,),
            Err(CommandError::NotFound(_))
        ));

        let source_id = project.sources[0].id.clone();
        let (project, event) = registry
            .remove_project_source(&project.id, &source_id, 6)
            .unwrap()
            .unwrap();
        assert!(matches!(event, DomainEvent::ProjectUpdated { .. }));
        assert_eq!(project.sources.len(), 1);
        assert!(project.sources[0].is_default);
        assert!(registry
            .remove_project_source(&project.id, &source_id, 7)
            .unwrap()
            .is_none());
    }

    #[test]
    fn a_project_with_a_thread_in_flight_cannot_be_archived() {
        let registry = registry();
        let project = personal(&registry);
        let (thread, _) = registry
            .create_thread(Some(project.clone()), None, None, 2)
            .unwrap();
        registry
            .post_message(&thread.id, MessageRole::User, "hi".into(), 3)
            .unwrap();
        assert_eq!(
            registry.thread(&thread.id).unwrap().status,
            ThreadStatus::Working
        );

        // Refuse, never cascade: the running thread keeps its project.
        assert!(matches!(
            registry.archive_project(&project, 4),
            Err(CommandError::Conflict(_))
        ));
        assert!(!registry.project(&project).unwrap().is_archived());
        assert_eq!(registry.thread(&thread.id).unwrap().project_id, project);
    }

    #[test]
    fn an_idle_thread_does_not_block_archiving_and_keeps_its_project() {
        let registry = registry();
        let project = personal(&registry);
        let (thread, _) = registry
            .create_thread(Some(project.clone()), None, None, 2)
            .unwrap();
        let (archived, event) = registry.archive_project(&project, 3).unwrap();
        assert!(archived.is_archived());
        assert!(matches!(event, DomainEvent::ProjectUpdated { .. }));
        // No cascade: the idle thread still belongs to the project.
        assert_eq!(registry.thread(&thread.id).unwrap().project_id, project);
        assert_eq!(
            registry.archive_project(&project, 4),
            Err(CommandError::Domain(DomainError::Archived {
                entity: "project"
            }))
        );
    }

    #[test]
    fn a_project_list_is_stably_sorted_with_active_first() {
        let registry = registry();
        // The seeded personal project is created at t=1.
        let (second, _) = registry
            .create_project("b".into(), ProjectKind::Standard, None, 2)
            .unwrap();
        let (third, _) = registry
            .create_project("c".into(), ProjectKind::Standard, None, 2)
            .unwrap();
        registry.archive_project(&second.id, 4).unwrap();

        let listed = registry.projects();
        assert_eq!(listed.len(), 3);
        // Active projects first — the seeded one (oldest) then `third` — and
        // the archived one last. `second` and `third` share a millisecond, so
        // the id is what makes that order deterministic.
        assert_eq!(listed[0].id, personal(&registry));
        assert!(!listed[0].is_archived());
        assert!(!listed[1].is_archived());
        assert!(listed[2].is_archived());
        assert_eq!(listed[2].id, second.id);
        // `third` is the only other active project, so it is second.
        assert_eq!(listed[1].id, third.id);

        // Every call returns the same order, not a `HashMap` iteration order.
        let again: Vec<ProjectId> = registry
            .projects()
            .into_iter()
            .map(|project| project.id)
            .collect();
        assert_eq!(
            again,
            listed.iter().map(|p| p.id.clone()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_thread_cannot_be_created_in_an_archived_project_before_archived() {
        let registry = registry();
        let (project, _) = registry
            .create_project("p".into(), ProjectKind::Standard, None, 2)
            .unwrap();
        registry.archive_project(&project.id, 3).unwrap();
        assert!(matches!(
            registry.create_thread(Some(project.id.clone()), None, None, 4),
            Err(CommandError::Conflict(_))
        ));
        assert!(matches!(
            registry.create_environment(
                Some(project.id),
                enrolled(&registry),
                EnvironmentKind::Unmanaged,
                Some("/srv/loom".into()),
                5,
            ),
            Err(CommandError::Conflict(_))
        ));
    }

    #[test]
    fn export_and_restore_round_trip_every_entity() {
        let registry = registry();
        let host = enrolled(&registry);
        let (environment, _) = registry
            .create_environment(
                Some(personal(&registry)),
                host,
                EnvironmentKind::Unmanaged,
                Some("/srv/loom".into()),
                3,
            )
            .unwrap();
        let (thread, _) = registry
            .create_thread(
                Some(personal(&registry)),
                Some("t".into()),
                Some(environment.id.clone()),
                4,
            )
            .unwrap();

        let snapshot = registry.export();
        let restored = DomainRegistry::new(9_999);
        restored.restore(snapshot.clone());

        assert_eq!(restored.export(), snapshot);
        assert_eq!(
            restored.thread(&thread.id).unwrap().title.as_deref(),
            Some("t")
        );
        assert!(restored.environment(&environment.id).is_some());
        assert_eq!(restored.hosts().len(), 1);
    }

    #[test]
    fn replaying_an_event_whose_entity_is_missing_is_a_noop() {
        let registry = registry();
        registry.apply_event(&DomainEvent::ThreadStatusChanged {
            thread_id: ThreadId::mint(),
            project_id: ProjectId::mint(),
            from: ThreadStatus::Idle,
            to: ThreadStatus::Working,
            at_ms: 5,
        });
        registry.apply_event(&DomainEvent::EnvironmentStatusChanged {
            environment_id: EnvironmentId::mint(),
            project_id: ProjectId::mint(),
            host_id: HostId::mint(),
            from: EnvironmentStatus::Creating,
            to: EnvironmentStatus::Ready,
            at_ms: 5,
        });
        registry.apply_event(&DomainEvent::HostStatusChanged {
            host_id: HostId::mint(),
            from: loom_domain::HostStatus::Connected,
            to: loom_domain::HostStatus::Disconnected,
            at_ms: 5,
        });
        // Nothing was invented and nothing panicked.
        assert!(registry.threads().is_empty());
        assert!(registry.environments().is_empty());
        assert!(registry.hosts().is_empty());
    }

    #[test]
    fn replaying_a_status_change_twice_lands_on_the_same_status() {
        let registry = registry();
        let (thread, _) = registry
            .create_thread(Some(personal(&registry)), None, None, 2)
            .unwrap();
        let event = DomainEvent::ThreadStatusChanged {
            thread_id: thread.id.clone(),
            project_id: thread.project_id.clone(),
            from: ThreadStatus::Idle,
            to: ThreadStatus::Working,
            at_ms: 3,
        };
        registry.apply_event(&event);
        registry.apply_event(&event);
        assert_eq!(
            registry.thread(&thread.id).unwrap().status,
            ThreadStatus::Working
        );
    }
}
