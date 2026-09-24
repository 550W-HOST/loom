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
    DomainError, DomainEvent, Environment, EnvironmentId, EnvironmentKind, EnvironmentSelection,
    EnvironmentStatus, EnvironmentTeardownOutcome, Host, HostId, Interaction, InteractionId,
    MessageRole, NewInteraction, NewQueuedMessage, NewThread, Project, ProjectId, ProjectKind,
    ProjectSourceId, ProviderSessionBinding, ProvisionedWorkspace, QueuedMessage, QueuedMessageId,
    QueuedMessageStatus, Resolution, RunId, Thread, ThreadId, ThreadOriginKind, ThreadSection,
    ThreadSectionId, ThreadStatus, ThreadTrigger, ThreadUpdate, GIT_WORKTREE_PROVIDER_ID,
};
use serde::{Deserialize, Serialize};

const ORDER_KEY_ALPHABET: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
const ORDER_KEY_WIDTH: usize = 16;
const ORDER_KEY_SPACE_EXHAUSTED: &str =
    "there is no order-key space between the requested neighbors";

fn create_order_key_between(
    previous_key: Option<&str>,
    next_key: Option<&str>,
) -> Result<String, CommandError> {
    let previous_key = previous_key.filter(|key| !key.is_empty());
    let next_key = next_key.filter(|key| !key.is_empty());
    for key in [previous_key, next_key].into_iter().flatten() {
        if key
            .bytes()
            .any(|digit| !ORDER_KEY_ALPHABET.contains(&digit))
        {
            return Err(CommandError::Conflict(format!(
                "invalid queued order key {key:?}"
            )));
        }
    }
    if previous_key.is_some_and(|left| next_key.is_some_and(|right| left >= right)) {
        return Err(CommandError::Conflict(
            "the requested order neighbors are not ordered".into(),
        ));
    }

    match (previous_key, next_key) {
        (None, None) => Ok("V".into()),
        (Some(previous), None) => Ok(format!("{previous}{}", ORDER_KEY_ALPHABET[0] as char)),
        (None, Some(next)) => {
            let first = next.as_bytes()[0];
            if first != ORDER_KEY_ALPHABET[0] {
                return Ok((ORDER_KEY_ALPHABET[0] as char).to_string());
            }
            if next.len() == 1 {
                return Err(CommandError::Conflict(ORDER_KEY_SPACE_EXHAUSTED.into()));
            }
            // A proper prefix sorts before `next`, and is the only available
            // shape when `next` starts with the alphabet's minimum digit.
            Ok(next[..next.len() - 1].to_owned())
        }
        (Some(previous), Some(next)) => {
            // Extending a key with the minimum digit is greater than the key
            // itself. It is also below `next` whenever the interval has room,
            // including when the keys first differ much later in the string.
            // The old digit-by-digit implementation accidentally returned a
            // key after `next` for adjacent-prefix pairs such as `0` and `00`.
            let candidate = format!("{previous}{}", ORDER_KEY_ALPHABET[0] as char);
            if candidate.as_str() < next {
                Ok(candidate)
            } else {
                Err(CommandError::Conflict(ORDER_KEY_SPACE_EXHAUSTED.into()))
            }
        }
    }
}

fn order_key_space_exhausted(error: &CommandError) -> bool {
    matches!(error, CommandError::Conflict(message) if message == ORDER_KEY_SPACE_EXHAUSTED)
}

fn order_key_is_occupied<'a, I>(key: &str, keys: I) -> bool
where
    I: IntoIterator<Item = &'a str>,
{
    keys.into_iter().any(|candidate| candidate == key)
}

/// Encodes a base-62 slot as a fixed-width key. Fixed-width keys make a
/// rebalanced order sparse again, so ordinary inserts can use the cheap
/// prefix-extension path for a long time before another rebalance is needed.
fn encode_order_key(mut value: u128) -> String {
    let mut digits = vec![ORDER_KEY_ALPHABET[0]; ORDER_KEY_WIDTH];
    let base = ORDER_KEY_ALPHABET.len() as u128;
    for digit in digits.iter_mut().rev() {
        *digit = ORDER_KEY_ALPHABET[(value % base) as usize];
        value /= base;
    }
    String::from_utf8(digits).expect("order-key alphabet is ASCII")
}

fn rebalance_order_keys(count: usize) -> Result<Vec<String>, CommandError> {
    let capacity = (ORDER_KEY_ALPHABET.len() as u128).pow(ORDER_KEY_WIDTH as u32);
    let slots = u128::try_from(count)
        .ok()
        .and_then(|count| count.checked_add(1))
        .ok_or_else(|| CommandError::Conflict("queued order is too large to rebalance".into()))?;
    let step = capacity / slots;
    if step == 0 {
        return Err(CommandError::Conflict(
            "queued order is too large to rebalance".into(),
        ));
    }
    (1..=count)
        .map(|index| {
            let value = step.checked_mul(index as u128).ok_or_else(|| {
                CommandError::Conflict("queued order is too large to rebalance".into())
            })?;
            Ok(encode_order_key(value))
        })
        .collect()
}

/// Assigns stable keys to rows written by a pre-B4 snapshot.
fn normalize_queued_order_keys(messages: &mut HashMap<QueuedMessageId, QueuedMessage>) {
    let mut thread_ids: Vec<ThreadId> = messages
        .values()
        .filter(|message| message.sort_key.is_empty())
        .map(|message| message.thread_id.clone())
        .collect();
    thread_ids.sort();
    thread_ids.dedup();
    for thread_id in thread_ids {
        let mut ordered: Vec<QueuedMessageId> = messages
            .values()
            .filter(|message| message.thread_id == thread_id)
            .map(|message| message.id.clone())
            .collect();
        ordered.sort_by(|left, right| {
            let left = messages.get(left).expect("queued id was collected");
            let right = messages.get(right).expect("queued id was collected");
            left.created_at_ms
                .cmp(&right.created_at_ms)
                .then_with(|| left.id.cmp(&right.id))
        });
        let keys = rebalance_order_keys(ordered.len())
            .expect("a snapshot queue must fit in the order-key space");
        for (id, key) in ordered.into_iter().zip(keys) {
            let message = messages.get_mut(&id).expect("queued id was collected");
            message.sort_key = key;
        }
    }
}

/// Orders projects the way the sidebar renders them.
///
/// Active before archived, then the client's explicit rank, then creation time
/// and id. A project whose rank was never set sorts after every ranked one but
/// still by creation time, so the list is deterministic without a migration
/// step: an unranked workspace looks exactly as it did before `projects.reorder`
/// existed.
fn compare_projects(left: &Project, right: &Project) -> std::cmp::Ordering {
    // The personal scope leads, whatever id it carries. It used to lead because
    // it was seeded with the earliest ULID — an accident of minting order that
    // stopped being true when its id became the reserved `proj_personal`, which
    // sorts after every ULID.
    let personal = |project: &Project| project.kind == ProjectKind::Personal;
    personal(right)
        .cmp(&personal(left))
        .then_with(|| left.is_archived().cmp(&right.is_archived()))
        .then_with(|| match (&left.sort_key, &right.sort_key) {
            (Some(left_key), Some(right_key)) => left_key.cmp(right_key),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        })
        .then_with(|| left.created_at_ms.cmp(&right.created_at_ms))
        .then_with(|| left.id.cmp(&right.id))
}

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

/// Projects, threads, hosts and environments held in memory.
#[derive(Debug)]
pub struct DomainRegistry {
    inner: Mutex<RegistryInner>,
}

/// A point-in-time copy of the registry, as stored in the entity view.
///
/// Vectors rather than maps so the serialized form is stable and diffable;
/// [`DomainRegistry::export`] sorts each one by id before returning.
///
/// The two fields added after version 1 — `queued_messages` and
/// `interactions` — are `#[serde(default)]`, so a snapshot written by an older
/// build still loads: it simply has no queue and no pending interaction, which
/// is exactly what that build would have described.
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
    /// Every queued message, in any status.
    #[serde(default)]
    pub queued_messages: Vec<QueuedMessage>,
    /// Every interaction, in any status.
    #[serde(default)]
    pub interactions: Vec<Interaction>,
    /// Every sidebar section, in any state.
    #[serde(default)]
    pub thread_sections: Vec<ThreadSection>,
}

#[derive(Debug)]
struct RegistryInner {
    personal_project_id: ProjectId,
    projects: HashMap<ProjectId, Project>,
    threads: HashMap<ThreadId, Thread>,
    hosts: HashMap<HostId, Host>,
    environments: HashMap<EnvironmentId, Environment>,
    queued_messages: HashMap<QueuedMessageId, QueuedMessage>,
    interactions: HashMap<InteractionId, Interaction>,
    thread_sections: HashMap<ThreadSectionId, ThreadSection>,
}

impl DomainRegistry {
    /// Creates the registry with the implicit personal project seeded.
    ///
    /// The personal project's id is the reserved `proj_personal`, not a minted
    /// one: it is the scope the client puts in a projectless thread route, so
    /// it has to be the same string on both sides and the same one on every
    /// start. A minted id made it a different project to the client after each
    /// restart.
    pub fn new(now_ms: u64) -> Self {
        let personal_project_id =
            ProjectId::sentinel().expect("the personal project reserves an id");
        let (project, _) = Project::create_with_id(
            personal_project_id.clone(),
            "Personal",
            ProjectKind::Personal,
            now_ms,
        )
        .expect("a personal project always has a valid name");
        let mut projects = HashMap::new();
        projects.insert(project.id.clone(), project);
        Self {
            inner: Mutex::new(RegistryInner {
                personal_project_id,
                projects,
                threads: HashMap::new(),
                hosts: HashMap::new(),
                environments: HashMap::new(),
                queued_messages: HashMap::new(),
                interactions: HashMap::new(),
                thread_sections: HashMap::new(),
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
    /// Sorted by the client's explicit rank when it set one, then by creation
    /// time and id. Active projects come first; archived ones follow, still
    /// sorted the same way. Deleted projects are omitted: a tombstone keeps the
    /// id resolvable for replay and for threads that still name it, but it is
    /// not part of the list a client renders.
    pub fn projects(&self) -> Vec<Project> {
        let mut projects: Vec<Project> = self
            .lock()
            .projects
            .values()
            .filter(|project| !project.is_deleted())
            .cloned()
            .collect();
        projects.sort_by(compare_projects);
        projects
    }

    /// Every project including deleted tombstones, for the sidebar's own
    /// lookups and for tests.
    pub fn all_projects(&self) -> Vec<Project> {
        let mut projects: Vec<Project> = self.lock().projects.values().cloned().collect();
        projects.sort_by(compare_projects);
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

    /// Soft-deletes a project and returns the update event.
    ///
    /// Like [`DomainRegistry::archive_project`] the busy check is the
    /// registry's, not the domain's. The rule is the same — refuse, never
    /// cascade — but a delete also refuses while *any* thread still belongs to
    /// the project, not only a running one: deleting a project whose idle
    /// threads would be stranded leaves those threads naming a project the
    /// client can no longer open. Archived threads are the exception, because
    /// they are already filed away and their history stays resolvable.
    ///
    /// Environments bound to the project get the same treatment as threads.
    pub fn delete_project(
        &self,
        project_id: &ProjectId,
        now_ms: u64,
    ) -> Result<(Project, DomainEvent), CommandError> {
        let mut inner = self.lock();
        let live_threads = inner
            .threads
            .values()
            .filter(|thread| &thread.project_id == project_id)
            .filter(|thread| thread.deleted_at_ms.is_none())
            .filter(|thread| thread.status != ThreadStatus::Archived)
            .count();
        let live_environments = inner
            .environments
            .values()
            .filter(|environment| &environment.project_id == project_id)
            .filter(|environment| environment.status != EnvironmentStatus::Destroyed)
            .count();
        if live_threads > 0 || live_environments > 0 {
            return Err(CommandError::Conflict(format!(
                "project {project_id} still has {live_threads} live thread(s) and \
                 {live_environments} live environment(s); archive or destroy them first"
            )));
        }
        let project = inner
            .projects
            .get_mut(project_id)
            .ok_or_else(|| CommandError::NotFound(format!("project {project_id} is not known")))?;
        if project.is_deleted() {
            return Err(CommandError::NotFound(format!(
                "project {project_id} is not known"
            )));
        }
        let event = project.delete(now_ms)?;
        Ok((project.clone(), event))
    }

    /// Updates one project source and returns the update event.
    ///
    /// `Ok(None)` means the source is not part of the project, which the HTTP
    /// layer reports as a `404`. `is_default` follows the contract's shape: the
    /// field is only ever `true`, so it promotes the source rather than
    /// demoting it, and the previous default steps down.
    pub fn update_project_source(
        &self,
        project_id: &ProjectId,
        source_id: &ProjectSourceId,
        path: Option<String>,
        is_default: bool,
        now_ms: u64,
    ) -> Result<Option<(Project, DomainEvent)>, CommandError> {
        let mut inner = self.lock();
        let project = inner
            .projects
            .get_mut(project_id)
            .ok_or_else(|| CommandError::NotFound(format!("project {project_id} is not known")))?;
        match project.update_source(source_id, path, is_default, now_ms)? {
            Some(event) => Ok(Some((project.clone(), event))),
            None => Ok(None),
        }
    }

    /// Moves a project between two ranked neighbours.
    ///
    /// A sparse base-62 key lets an insert between two rows rewrite one record
    /// instead of renumbering the list — but only when the neighbours already
    /// carry keys. A project that has never been reordered has none, and
    /// synthesising a key for it would place the new rank *before* every
    /// unranked project rather than between the two rows the client named. So
    /// the first reorder, and any reorder whose neighbours are unranked, falls
    /// back to rewriting the whole visible list; after that every project holds
    /// a key and the cheap path applies.
    ///
    /// A neighbour that is not in the current order, or a pair already in the
    /// requested order, is a conflict rather than a silent no-op the caller
    /// cannot detect.
    ///
    /// Returns the newly ordered projects and the events a subscriber needs.
    pub fn reorder_project(
        &self,
        project_id: &ProjectId,
        previous_project_id: Option<&ProjectId>,
        next_project_id: Option<&ProjectId>,
        now_ms: u64,
    ) -> Result<(Vec<Project>, Vec<DomainEvent>), CommandError> {
        let mut inner = self.lock();
        let moved = inner
            .projects
            .get(project_id)
            .ok_or_else(|| CommandError::NotFound(format!("project {project_id} is not known")))?;
        if moved.is_deleted() {
            return Err(CommandError::NotFound(format!(
                "project {project_id} is not known"
            )));
        }
        let archived = moved.is_archived();
        // Only projects a client can actually see and drag: archived projects
        // are rendered in their own group, so ordering one against an active
        // neighbour has no meaning.
        let mut ordered: Vec<Project> = inner
            .projects
            .values()
            .filter(|project| project.is_archived() == archived && !project.is_deleted())
            .cloned()
            .collect();
        ordered.sort_by(compare_projects);

        let neighbor = |id: Option<&ProjectId>| -> Result<Option<&Project>, CommandError> {
            let Some(id) = id else {
                return Ok(None);
            };
            if id == project_id {
                return Err(CommandError::Conflict(
                    "a project cannot be its own neighbor".into(),
                ));
            }
            ordered
                .iter()
                .find(|project| project.id == *id)
                .map(Some)
                .ok_or_else(|| CommandError::Conflict("project neighbor is stale".into()))
        };
        let previous = neighbor(previous_project_id)?;
        let next = neighbor(next_project_id)?;
        // The neighbours must name an ordered pair in the list as the client
        // sees it. Comparing ids was a shortcut that held only while every id
        // was a ULID minted in creation order: the reserved personal id sorts
        // outside that, and a reorder moves a project away from the position
        // its id implies. Read the positions instead.
        if let (Some(previous), Some(next)) = (previous, next) {
            let position = |project: &Project| {
                ordered
                    .iter()
                    .position(|candidate| candidate.id == project.id)
                    .expect("a neighbour is a member of the ordered list")
            };
            if position(previous) >= position(next) {
                return Err(CommandError::Conflict(
                    "the requested neighbors are not ordered".into(),
                ));
            }
        }

        // The order the client asked for, computed without touching anything:
        // the current list minus the moved project, with it re-inserted between
        // its two named neighbours.
        let mut desired_ids: Vec<ProjectId> = ordered
            .iter()
            .filter(|project| project.id != *project_id)
            .map(|project| project.id.clone())
            .collect();
        let insert_index = match desired_ids
            .iter()
            .position(|id| Some(id) == previous_project_id)
        {
            Some(index) => index + 1,
            None => desired_ids
                .iter()
                .position(|id| Some(id) == next_project_id)
                .unwrap_or_default(),
        };
        desired_ids.insert(insert_index, project_id.clone());

        // The projects bounding the insertion point, from the list the moved
        // project was removed from.
        let bounding_previous = insert_index
            .checked_sub(1)
            .and_then(|index| desired_ids.get(index))
            .and_then(|id| inner.projects.get(id))
            .cloned();
        let bounding_next = desired_ids
            .get(insert_index + 1)
            .and_then(|id| inner.projects.get(id))
            .cloned();

        let mut events = Vec::new();
        let cheap = create_order_key_between(
            bounding_previous
                .as_ref()
                .and_then(|p| p.sort_key.as_deref()),
            bounding_next.as_ref().and_then(|p| p.sort_key.as_deref()),
        );
        // The cheap path is only sound when both bounding projects already hold
        // keys (a missing bound is the list edge, which is always fine): a
        // synthesised key for an unranked neighbour would sort before every
        // unranked project rather than between the two named rows.
        let bounding_ranked = bounding_previous
            .as_ref()
            .is_none_or(|p| p.sort_key.is_some())
            && bounding_next.as_ref().is_none_or(|p| p.sort_key.is_some());
        let cheap_key = match cheap {
            Ok(key) if bounding_ranked => Some(key),
            _ => None,
        };

        if let Some(key) = cheap_key {
            let moved = inner
                .projects
                .get_mut(project_id)
                .expect("the project was present in the registry");
            if let Some(event) = moved.set_sort_key(Some(key), now_ms)? {
                events.push(event);
            }
        } else {
            // A full rebalance: every visible project is rewritten into the
            // desired order. After this every one of them carries a key, so the
            // next reorder takes the cheap path.
            let keys = rebalance_order_keys(desired_ids.len())?;
            for (id, key) in desired_ids.iter().zip(keys) {
                let project = inner
                    .projects
                    .get_mut(id)
                    .expect("the project was collected");
                if let Some(event) = project.set_sort_key(Some(key), now_ms)? {
                    events.push(event);
                }
            }
        }

        let mut reordered: Vec<Project> = inner
            .projects
            .values()
            .filter(|project| project.is_archived() == archived && !project.is_deleted())
            .cloned()
            .collect();
        reordered.sort_by(compare_projects);
        Ok((reordered, events))
    }

    /// Creates a section and returns it with its event.
    ///
    /// A name that is already taken is a conflict, not a second row: the
    /// contract declares `409` for exactly this case.
    pub fn create_thread_section(
        &self,
        name: String,
        now_ms: u64,
    ) -> Result<(ThreadSection, DomainEvent), CommandError> {
        let mut inner = self.lock();
        let (section, event) = ThreadSection::create(name, now_ms)?;
        if let Some(existing) = inner
            .thread_sections
            .values()
            .find(|candidate| candidate.name == section.name)
        {
            return Err(CommandError::Conflict(format!(
                "a section named {:?} already exists ({})",
                section.name, existing.id
            )));
        }
        inner
            .thread_sections
            .insert(section.id.clone(), section.clone());
        Ok((section, event))
    }

    /// Renames a section and returns the updated section with its event.
    ///
    /// Renaming to a name another section already holds is the contract's
    /// `409`; renaming to the section's own name is an idempotent success.
    pub fn update_thread_section(
        &self,
        section_id: &ThreadSectionId,
        name: String,
        now_ms: u64,
    ) -> Result<(ThreadSection, DomainEvent), CommandError> {
        let mut inner = self.lock();
        let section_name = {
            let section = inner.thread_sections.get_mut(section_id).ok_or_else(|| {
                CommandError::NotFound(format!("section {section_id} is not known"))
            })?;
            section.rename(name, now_ms)?;
            section.name.clone()
        };
        let taken = inner
            .thread_sections
            .values()
            .any(|candidate| candidate.id != *section_id && candidate.name == section_name);
        if taken {
            return Err(CommandError::Conflict(format!(
                "a section named {section_name:?} already exists"
            )));
        }
        let section = inner
            .thread_sections
            .get(section_id)
            .expect("the section was checked above")
            .clone();
        let event = DomainEvent::ThreadSectionUpdated {
            section: section.clone(),
        };
        Ok((section, event))
    }

    /// Deletes a section and returns what it was, with how many threads
    /// referenced it.
    ///
    /// The threads are counted, not rewritten: a delete re-filing every thread
    /// would mutate an entity the client did not ask about. See
    /// [`crate::b7`] and `docs/projects.md`.
    pub fn delete_thread_section(
        &self,
        section_id: &ThreadSectionId,
        now_ms: u64,
    ) -> Result<(ThreadSection, usize, DomainEvent), CommandError> {
        let mut inner = self.lock();
        let section = inner
            .thread_sections
            .remove(section_id)
            .ok_or_else(|| CommandError::NotFound(format!("section {section_id} is not known")))?;
        let section_key = section.id.to_string();
        let updated_thread_count = inner
            .threads
            .values()
            .filter(|thread| thread.deleted_at_ms.is_none())
            .filter(|thread| thread.section_id.as_deref() == Some(section_key.as_str()))
            .count();
        let event = DomainEvent::ThreadSectionDeleted {
            section_id: section.id.clone(),
            name: section.name.clone(),
        };
        let _ = now_ms;
        Ok((section, updated_thread_count, event))
    }

    /// Every known section, newest first and then by id.
    pub fn thread_sections(&self) -> Vec<ThreadSection> {
        let mut sections: Vec<ThreadSection> =
            self.lock().thread_sections.values().cloned().collect();
        sections.sort_by(|left, right| {
            left.created_at_ms
                .cmp(&right.created_at_ms)
                .then_with(|| left.id.cmp(&right.id))
        });
        sections
    }

    /// One section by id.
    pub fn thread_section(&self, section_id: &ThreadSectionId) -> Option<ThreadSection> {
        self.lock().thread_sections.get(section_id).cloned()
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

    /// Creates a child thread with fork provenance, without copying the
    /// source provider session. The caller must decide whether the provider
    /// can actually fork; this method only records a successful domain create.
    pub fn create_fork_thread(
        &self,
        source_thread_id: &ThreadId,
        title: Option<String>,
        environment_id: Option<EnvironmentId>,
        origin_plugin_id: Option<String>,
        now_ms: u64,
    ) -> Result<(Thread, DomainEvent), CommandError> {
        let mut inner = self.lock();
        let source = inner
            .threads
            .get(source_thread_id)
            .cloned()
            .filter(|thread| thread.deleted_at_ms.is_none())
            .ok_or_else(|| {
                CommandError::NotFound(format!("thread {source_thread_id} is not known"))
            })?;
        let Some(project) = inner.projects.get(&source.project_id) else {
            return Err(CommandError::NotFound(format!(
                "project {} is not known",
                source.project_id
            )));
        };
        if project.is_archived() {
            return Err(CommandError::Conflict(format!(
                "project {} is archived; unarchive it before creating a thread",
                source.project_id
            )));
        }
        if let Some(id) = &environment_id {
            if !inner.environments.contains_key(id) {
                return Err(CommandError::NotFound(format!(
                    "environment {id} is not known"
                )));
            }
        }
        let (mut thread, _) = Thread::create(
            NewThread {
                project_id: source.project_id,
                title,
                parent_thread_id: Some(source.id.clone()),
                environment_id,
            },
            now_ms,
        );
        thread.source_thread_id = Some(source.id);
        thread.origin_kind = Some(ThreadOriginKind::Fork);
        thread.origin_plugin_id = origin_plugin_id;
        let event = DomainEvent::ThreadCreated {
            thread: thread.clone(),
        };
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
    /// one starts `creating` with none, and is provisioned later by a worker.
    pub fn create_environment(
        &self,
        project_id: Option<ProjectId>,
        host_id: HostId,
        kind: EnvironmentKind,
        path: Option<String>,
        now_ms: u64,
    ) -> Result<(Environment, Vec<DomainEvent>), CommandError> {
        self.create_environment_with(
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
    /// On top of the kind's invariants, a `git-worktree` environment requires
    /// the project to have a checked-out source on the environment's host:
    /// the worker has nothing to cut a worktree from otherwise, and failing at
    /// creation is clearer than dispatching a provisioning request that can
    /// only fail.
    pub fn create_environment_with(
        &self,
        project_id: Option<ProjectId>,
        host_id: HostId,
        kind: EnvironmentKind,
        path: Option<String>,
        selection: EnvironmentSelection,
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
        let provider_id = selection
            .provider_id
            .as_deref()
            .map(str::trim)
            .filter(|provider| !provider.is_empty())
            .unwrap_or_else(|| kind.default_provider_id());
        if provider_id == GIT_WORKTREE_PROVIDER_ID
            && !project
                .sources
                .iter()
                .any(|source| source.host_id == host_id && !source.path.trim().is_empty())
        {
            return Err(CommandError::Conflict(format!(
                "project {project_id} has no checked-out source on host {host_id} to cut a worktree from"
            )));
        }
        let (environment, event) =
            Environment::create_with(project_id, host_id, kind, path, selection, now_ms)?;
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

    /// Records what a worker provisioned and reaches `ready` while the
    /// registry lock is held. This prevents deletion or a second report from
    /// landing between the path write and the lifecycle transition.
    ///
    /// The events include the whole-environment update that carries the path
    /// and branch, so replay after a restart restores the ownership record and
    /// not only the status.
    pub fn complete_environment_provisioning(
        &self,
        environment_id: &EnvironmentId,
        workspace: ProvisionedWorkspace,
        now_ms: u64,
    ) -> Result<(Environment, Vec<DomainEvent>), CommandError> {
        let mut inner = self.lock();
        let environment = inner.environments.get_mut(environment_id).ok_or_else(|| {
            CommandError::NotFound(format!("environment {environment_id} is not known"))
        })?;
        let events = environment.complete_provisioning(workspace, now_ms)?;
        Ok((environment.clone(), events))
    }

    /// Begins a managed environment's teardown: `destroyed` plus a `running`
    /// teardown record, as one state-machine operation.
    ///
    /// A retry on an already destroyed environment is allowed and increments
    /// the attempt; the returned events make the whole record durable.
    pub fn begin_environment_teardown(
        &self,
        environment_id: &EnvironmentId,
        now_ms: u64,
    ) -> Result<(Environment, Vec<DomainEvent>), CommandError> {
        let mut inner = self.lock();
        let environment = inner.environments.get_mut(environment_id).ok_or_else(|| {
            CommandError::NotFound(format!("environment {environment_id} is not known"))
        })?;
        let events = environment.begin_teardown(now_ms)?;
        Ok((environment.clone(), events))
    }

    /// Records the outcome of an in-flight teardown.
    ///
    /// A report for a teardown that never started, or already settled, is a
    /// `CommandError::Domain`; the caller maps that to a stale report.
    pub fn complete_environment_teardown(
        &self,
        environment_id: &EnvironmentId,
        outcome: EnvironmentTeardownOutcome,
        now_ms: u64,
    ) -> Result<(Environment, DomainEvent), CommandError> {
        let mut inner = self.lock();
        let environment = inner.environments.get_mut(environment_id).ok_or_else(|| {
            CommandError::NotFound(format!("environment {environment_id} is not known"))
        })?;
        let event = environment.complete_teardown(outcome, now_ms)?;
        Ok((environment.clone(), event))
    }

    /// Updates environment metadata and returns the project-scoped event.
    pub fn update_environment(
        &self,
        environment_id: &EnvironmentId,
        name: Option<Option<String>>,
        merge_base_branch: Option<Option<String>>,
        now_ms: u64,
    ) -> Result<(Environment, DomainEvent), CommandError> {
        let mut inner = self.lock();
        let environment = inner.environments.get_mut(environment_id).ok_or_else(|| {
            CommandError::NotFound(format!("environment {environment_id} is not known"))
        })?;
        if environment.status == EnvironmentStatus::Destroyed {
            return Err(CommandError::Conflict(format!(
                "environment {environment_id} is destroyed"
            )));
        }
        let event = environment.update(name, merge_base_branch, now_ms)?;
        Ok((environment.clone(), event))
    }

    /// Archives every non-deleted thread bound to an environment.
    ///
    /// A running thread is refused as a unit: archiving it would leave an
    /// in-flight provider pointed at an environment the caller is trying to
    /// retire. Already archived rows are omitted from the returned ids.
    pub fn archive_environment_threads(
        &self,
        environment_id: &EnvironmentId,
        now_ms: u64,
    ) -> Result<(Vec<ThreadId>, Vec<DomainEvent>), CommandError> {
        let mut inner = self.lock();
        let environment = inner.environments.get(environment_id).ok_or_else(|| {
            CommandError::NotFound(format!("environment {environment_id} is not known"))
        })?;
        if environment.status == EnvironmentStatus::Destroyed {
            return Err(CommandError::Conflict(format!(
                "environment {environment_id} is destroyed"
            )));
        }
        let busy = inner
            .threads
            .values()
            .filter(|thread| {
                thread.deleted_at_ms.is_none()
                    && thread.environment_id.as_ref() == Some(environment_id)
                    && matches!(thread.status, ThreadStatus::Working | ThreadStatus::Waiting)
            })
            .count();
        if busy > 0 {
            return Err(CommandError::Conflict(format!(
                "environment {environment_id} has {busy} thread(s) with a run in flight"
            )));
        }

        let mut ids: Vec<ThreadId> = inner
            .threads
            .values()
            .filter(|thread| {
                thread.deleted_at_ms.is_none()
                    && thread.environment_id.as_ref() == Some(environment_id)
                    && thread.status != ThreadStatus::Archived
            })
            .map(|thread| thread.id.clone())
            .collect();
        ids.sort();
        let mut archived_ids = Vec::with_capacity(ids.len());
        let mut events = Vec::with_capacity(ids.len());
        for id in ids {
            let thread = inner
                .threads
                .get_mut(&id)
                .expect("thread ids were collected from the map");
            let event = thread.transition(ThreadTrigger::Archive, now_ms)?;
            archived_ids.push(id);
            events.push(event);
        }
        Ok((archived_ids, events))
    }

    /// Retires an environment after its bound threads have been archived.
    ///
    /// The record remains as a destroyed tombstone so replay and snapshots do
    /// not resurrect a workspace the caller explicitly removed. Filesystem
    /// teardown is host-owned and is intentionally a separate RPC concern.
    pub fn delete_environment(
        &self,
        environment_id: &EnvironmentId,
        now_ms: u64,
    ) -> Result<(Environment, Option<DomainEvent>), CommandError> {
        let mut inner = self.lock();
        let current_status = inner
            .environments
            .get(environment_id)
            .ok_or_else(|| {
                CommandError::NotFound(format!("environment {environment_id} is not known"))
            })?
            .status;
        if current_status == EnvironmentStatus::Destroyed {
            return Ok((
                inner
                    .environments
                    .get(environment_id)
                    .expect("environment was checked above")
                    .clone(),
                None,
            ));
        }
        let has_bound_threads = inner.threads.values().any(|thread| {
            thread.deleted_at_ms.is_none()
                && thread.environment_id.as_ref() == Some(environment_id)
                && thread.status != ThreadStatus::Archived
        });
        if has_bound_threads {
            return Err(CommandError::Conflict(format!(
                "environment {environment_id} still has active threads; archive them first"
            )));
        }
        let environment = inner
            .environments
            .get_mut(environment_id)
            .expect("environment was checked above");
        let event = environment.set_status(EnvironmentStatus::Destroyed, now_ms)?;
        Ok((environment.clone(), Some(event)))
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
        if thread.deleted_at_ms.is_some() {
            return Err(CommandError::NotFound(format!(
                "thread {thread_id} is not known"
            )));
        }
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

    /// Enrolls a worker as a host, idempotently.
    ///
    /// This is the operation a worker performs when it connects, and it is
    /// why a worker's identity survives reconnects:
    ///
    /// * `host_id: Some(id)` and the host is known — it is marked connected
    ///   and, if it was disconnected, a `host_status_changed` event is
    ///   produced. No second machine is created.
    /// * `host_id: Some(id)` and the host is unknown — it is created under the
    ///   identity the worker supplied, which is how a server started after the
    ///   worker still recognises it.
    /// * `host_id: None` — a fresh identity is minted, for a worker that has
    ///   never enrolled before.
    ///
    /// `data_dir` is the machine's own data directory. It is recorded — never
    /// cleared by an omission — because thread storage is named from it and a
    /// storage read should not start failing because one enrollment left it
    /// out.
    pub fn enroll_host(
        &self,
        host_id: Option<HostId>,
        name: String,
        now_ms: u64,
    ) -> Result<(Host, Vec<DomainEvent>), CommandError> {
        self.enroll_host_with_data_dir(host_id, name, None, now_ms)
    }

    /// [`DomainRegistry::enroll_host`] with the worker's reported data
    /// directory.
    pub fn enroll_host_with_data_dir(
        &self,
        host_id: Option<HostId>,
        name: String,
        data_dir: Option<String>,
        now_ms: u64,
    ) -> Result<(Host, Vec<DomainEvent>), CommandError> {
        let mut inner = self.lock();
        if let Some(id) = host_id {
            if let Some(existing) = inner.hosts.get_mut(&id) {
                let data_dir_changed = existing.record_data_dir(data_dir.as_deref(), now_ms);
                let status_event = existing.mark_connected(now_ms);
                let mut events = Vec::with_capacity(usize::from(data_dir_changed) + 1);
                if data_dir_changed {
                    events.push(DomainEvent::HostUpdated {
                        host: existing.clone(),
                    });
                }
                if let Some(event) = status_event {
                    events.push(event);
                }
                return Ok((existing.clone(), events));
            }
            let (host, event) = Host::register_with_data_dir(Some(id), name, data_dir, now_ms)?;
            inner.hosts.insert(host.id.clone(), host.clone());
            return Ok((host, vec![event]));
        }
        let (host, event) = Host::register_with_data_dir(None, name, data_dir, now_ms)?;
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

    /// Marks a host's worker detached, returning an event on an actual change.
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
    /// worker the answer falls back to a connected remote host, and with no
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

    /// Looks up a thread that is still public to clients.
    pub fn public_thread(&self, thread_id: &ThreadId) -> Option<Thread> {
        self.lock()
            .threads
            .get(thread_id)
            .filter(|thread| thread.deleted_at_ms.is_none())
            .cloned()
    }

    /// Marks a thread read and returns the replayable metadata event.
    pub fn mark_thread_read(
        &self,
        thread_id: &ThreadId,
        now_ms: u64,
    ) -> Result<(Thread, Option<DomainEvent>), CommandError> {
        let mut inner = self.lock();
        let thread = inner
            .threads
            .get_mut(thread_id)
            .ok_or_else(|| CommandError::NotFound(format!("thread {thread_id} is not known")))?;
        let event = thread.set_read_at(Some(now_ms), now_ms);
        Ok((thread.clone(), event))
    }

    /// Clears a thread's read marker and returns the replayable metadata event.
    pub fn mark_thread_unread(
        &self,
        thread_id: &ThreadId,
        now_ms: u64,
    ) -> Result<(Thread, Option<DomainEvent>), CommandError> {
        let mut inner = self.lock();
        let thread = inner
            .threads
            .get_mut(thread_id)
            .ok_or_else(|| CommandError::NotFound(format!("thread {thread_id} is not known")))?;
        let event = thread.set_read_at(None, now_ms);
        Ok((thread.clone(), event))
    }

    /// Applies a client's field changes to a stored thread.
    ///
    /// Returns the thread after the change and the event to publish, or no
    /// event at all when the update changed nothing. Parentage is validated
    /// here rather than in the domain, because "does this thread exist and is
    /// it in the same project" needs the other threads.
    pub fn update_thread(
        &self,
        thread_id: &ThreadId,
        update: &ThreadUpdate,
        now_ms: u64,
    ) -> Result<(Thread, Option<DomainEvent>), CommandError> {
        let mut inner = self.lock();
        let Some(project_id) = inner
            .threads
            .get(thread_id)
            .map(|thread| thread.project_id.clone())
        else {
            return Err(CommandError::NotFound(format!(
                "thread {thread_id} is not known"
            )));
        };
        if let Some(Some(parent_id)) = &update.parent_thread_id {
            if parent_id == thread_id {
                return Err(CommandError::Conflict(format!(
                    "thread {thread_id} cannot be its own parent"
                )));
            }
            let Some(parent) = inner.threads.get(parent_id) else {
                return Err(CommandError::NotFound(format!(
                    "parent thread {parent_id} is not known"
                )));
            };
            if parent.project_id != project_id {
                return Err(CommandError::Conflict(format!(
                    "parent thread {parent_id} belongs to a different project"
                )));
            }
        }
        let thread = inner
            .threads
            .get_mut(thread_id)
            .expect("the thread was just looked up");
        let event = thread.apply_update(update, now_ms)?;
        Ok((thread.clone(), event))
    }

    /// Replaces a thread's tabs under a compare-and-swap revision.
    ///
    /// A stale `expected_revision` is [`CommandError::Conflict`], which the
    /// route reports as bb's `thread_tabs_conflict`.
    pub fn set_thread_tabs(
        &self,
        thread_id: &ThreadId,
        tabs: Vec<serde_json::Value>,
        expected_revision: u64,
        now_ms: u64,
    ) -> Result<(Thread, DomainEvent), CommandError> {
        let mut inner = self.lock();
        let thread = inner
            .threads
            .get_mut(thread_id)
            .ok_or_else(|| CommandError::NotFound(format!("thread {thread_id} is not known")))?;
        let event = match thread.set_tabs(tabs, expected_revision, now_ms) {
            Ok(event) => event,
            // A stale revision is a registry-level conflict: the write was
            // well-formed and the client's view was not current. Keeping it a
            // `Conflict` is what lets the route answer bb's
            // `thread_tabs_conflict` without matching on domain internals.
            Err(error @ DomainError::TabsConflict { .. }) => {
                return Err(CommandError::Conflict(error.to_string()))
            }
            Err(error) => return Err(CommandError::Domain(error)),
        };
        Ok((thread.clone(), event))
    }

    /// How many threads name `thread_id` as their parent.
    ///
    /// bb's `childSummary` counts *non-deleted* children, so tombstones do not
    /// keep a parent looking as if it still has a live child.
    pub fn child_count(&self, thread_id: &ThreadId) -> usize {
        self.lock()
            .threads
            .values()
            .filter(|thread| {
                thread.deleted_at_ms.is_none()
                    && thread.parent_thread_id.as_ref() == Some(thread_id)
            })
            .count()
    }

    /// Every non-deleted thread, newest first.
    ///
    /// The list a UI renders in its sidebar. Ordering is by creation time and
    /// then id, so it is stable when two threads share a millisecond.
    pub fn threads(&self) -> Vec<Thread> {
        let mut threads: Vec<Thread> = self
            .lock()
            .threads
            .values()
            .filter(|thread| thread.deleted_at_ms.is_none())
            .cloned()
            .collect();
        threads.sort_by(|left, right| {
            right
                .created_at_ms
                .cmp(&left.created_at_ms)
                .then_with(|| left.id.cmp(&right.id))
        });
        threads
    }

    /// Archives one live thread. Repeating the command is an idempotent
    /// success, matching the lifecycle route's acknowledgement contract.
    pub fn archive_thread(
        &self,
        thread_id: &ThreadId,
        now_ms: u64,
    ) -> Result<(Thread, Option<DomainEvent>), CommandError> {
        let mut inner = self.lock();
        let thread = inner
            .threads
            .get_mut(thread_id)
            .ok_or_else(|| CommandError::NotFound(format!("thread {thread_id} is not known")))?;
        if thread.deleted_at_ms.is_some() {
            return Err(CommandError::NotFound(format!(
                "thread {thread_id} is not known"
            )));
        }
        if thread.status == ThreadStatus::Archived {
            return Ok((thread.clone(), None));
        }
        let event = thread.transition(ThreadTrigger::Archive, now_ms)?;
        Ok((thread.clone(), Some(event)))
    }

    /// Archives the target and its direct child/source-fork threads.
    ///
    /// The returned ids contain only rows that changed during this command;
    /// deleted tombstones and already archived rows are deliberately omitted.
    pub fn archive_all_threads(
        &self,
        thread_id: &ThreadId,
        now_ms: u64,
    ) -> Result<(Vec<ThreadId>, Vec<DomainEvent>), CommandError> {
        let mut inner = self.lock();
        let target = inner
            .threads
            .get(thread_id)
            .ok_or_else(|| CommandError::NotFound(format!("thread {thread_id} is not known")))?;
        if target.deleted_at_ms.is_some() {
            return Err(CommandError::NotFound(format!(
                "thread {thread_id} is not known"
            )));
        }

        let mut ids = vec![thread_id.clone()];
        ids.extend(
            inner
                .threads
                .values()
                .filter(|thread| {
                    thread.id != *thread_id
                        && thread.deleted_at_ms.is_none()
                        && (thread.parent_thread_id.as_ref() == Some(thread_id)
                            || thread.source_thread_id.as_ref() == Some(thread_id))
                })
                .map(|thread| thread.id.clone()),
        );
        ids.sort_by(|left, right| {
            if left == thread_id {
                std::cmp::Ordering::Less
            } else if right == thread_id {
                std::cmp::Ordering::Greater
            } else {
                left.cmp(right)
            }
        });

        let mut archived_ids = Vec::new();
        let mut events = Vec::new();
        for id in ids {
            let Some(thread) = inner.threads.get_mut(&id) else {
                continue;
            };
            if thread.deleted_at_ms.is_some() || thread.status == ThreadStatus::Archived {
                continue;
            }
            let event = thread.transition(ThreadTrigger::Archive, now_ms)?;
            archived_ids.push(id);
            events.push(event);
        }
        Ok((archived_ids, events))
    }

    /// Soft-deletes a thread while retaining its tombstone for replay.
    pub fn delete_thread(
        &self,
        thread_id: &ThreadId,
        now_ms: u64,
    ) -> Result<(Thread, Option<DomainEvent>), CommandError> {
        let mut inner = self.lock();
        let thread = inner
            .threads
            .get_mut(thread_id)
            .ok_or_else(|| CommandError::NotFound(format!("thread {thread_id} is not known")))?;
        if thread.deleted_at_ms.is_some() {
            return Err(CommandError::NotFound(format!(
                "thread {thread_id} is not known"
            )));
        }
        let event = thread.mark_deleted(now_ms);
        Ok((thread.clone(), event))
    }

    /// Unarchives a live thread. Repeating the command is an idempotent
    /// success, so an already idle thread returns without an event.
    pub fn unarchive_thread(
        &self,
        thread_id: &ThreadId,
        now_ms: u64,
    ) -> Result<(Thread, Option<DomainEvent>), CommandError> {
        let mut inner = self.lock();
        let thread = inner
            .threads
            .get_mut(thread_id)
            .ok_or_else(|| CommandError::NotFound(format!("thread {thread_id} is not known")))?;
        if thread.deleted_at_ms.is_some() {
            return Err(CommandError::NotFound(format!(
                "thread {thread_id} is not known"
            )));
        }
        if thread.status != ThreadStatus::Archived {
            return Ok((thread.clone(), None));
        }
        let event = thread.transition(ThreadTrigger::Unarchive, now_ms)?;
        Ok((thread.clone(), Some(event)))
    }

    /// Pins a thread at the front of the pinned list.
    pub fn pin_thread(
        &self,
        thread_id: &ThreadId,
        now_ms: u64,
    ) -> Result<(Thread, Vec<DomainEvent>), CommandError> {
        let mut inner = self.lock();
        let thread =
            inner.threads.get(thread_id).cloned().ok_or_else(|| {
                CommandError::NotFound(format!("thread {thread_id} is not known"))
            })?;
        if thread.deleted_at_ms.is_some() {
            return Err(CommandError::NotFound(format!(
                "thread {thread_id} is not known"
            )));
        }
        if thread.pinned_at_ms.is_some() && thread.pin_sort_key.is_some() {
            return Ok((thread, Vec::new()));
        }
        let first_key = inner
            .threads
            .values()
            .filter(|thread| {
                thread.deleted_at_ms.is_none()
                    && thread.pinned_at_ms.is_some()
                    && thread.pin_sort_key.is_some()
            })
            .min_by(|left, right| {
                left.pin_sort_key
                    .cmp(&right.pin_sort_key)
                    .then_with(|| left.id.cmp(&right.id))
            })
            .and_then(|thread| thread.pin_sort_key.clone());
        let sort_key = match create_order_key_between(None, first_key.as_deref()) {
            Ok(sort_key)
                if order_key_is_occupied(
                    &sort_key,
                    inner
                        .threads
                        .values()
                        .filter(|candidate| candidate.id != *thread_id)
                        .filter_map(|candidate| candidate.pin_sort_key.as_deref()),
                ) =>
            {
                Err(CommandError::Conflict(ORDER_KEY_SPACE_EXHAUSTED.into()))
            }
            result => result,
        };
        let sort_key = match sort_key {
            Ok(sort_key) => sort_key,
            Err(error) if order_key_space_exhausted(&error) => {
                let mut ids: Vec<ThreadId> = inner
                    .threads
                    .values()
                    .filter(|candidate| {
                        candidate.id != *thread_id
                            && candidate.deleted_at_ms.is_none()
                            && candidate.pinned_at_ms.is_some()
                            && candidate.pin_sort_key.is_some()
                    })
                    .map(|candidate| candidate.id.clone())
                    .collect();
                ids.sort_by(|left, right| {
                    inner
                        .threads
                        .get(left)
                        .and_then(|thread| thread.pin_sort_key.as_ref())
                        .cmp(
                            &inner
                                .threads
                                .get(right)
                                .and_then(|thread| thread.pin_sort_key.as_ref()),
                        )
                        .then_with(|| left.cmp(right))
                });
                ids.insert(0, thread_id.clone());
                let keys = rebalance_order_keys(ids.len())?;
                let mut events = Vec::new();
                for (id, key) in ids.into_iter().zip(keys) {
                    let candidate = inner
                        .threads
                        .get_mut(&id)
                        .expect("the pinned thread was collected");
                    let pinned_at = if id == *thread_id {
                        Some(now_ms)
                    } else {
                        candidate.pinned_at_ms
                    };
                    if let Some(event) = candidate.set_pin(pinned_at, Some(key), now_ms) {
                        events.push(event);
                    }
                }
                let thread = inner
                    .threads
                    .get(thread_id)
                    .expect("the target was present in the registry")
                    .clone();
                return Ok((thread, events));
            }
            Err(error) => return Err(error),
        };
        let thread = inner
            .threads
            .get_mut(thread_id)
            .expect("the target was present in the registry");
        let event = thread.set_pin(Some(now_ms), Some(sort_key), now_ms);
        Ok((thread.clone(), event.into_iter().collect()))
    }

    /// Removes a thread from the pinned list.
    pub fn unpin_thread(
        &self,
        thread_id: &ThreadId,
        now_ms: u64,
    ) -> Result<(Thread, Option<DomainEvent>), CommandError> {
        let mut inner = self.lock();
        let thread = inner
            .threads
            .get_mut(thread_id)
            .ok_or_else(|| CommandError::NotFound(format!("thread {thread_id} is not known")))?;
        if thread.deleted_at_ms.is_some() {
            return Err(CommandError::NotFound(format!(
                "thread {thread_id} is not known"
            )));
        }
        let event = thread.set_pin(None, None, now_ms);
        Ok((thread.clone(), event))
    }

    /// Moves a pinned thread between the supplied neighbors.
    pub fn reorder_pinned_thread(
        &self,
        thread_id: &ThreadId,
        previous_thread_id: Option<&ThreadId>,
        next_thread_id: Option<&ThreadId>,
        now_ms: u64,
    ) -> Result<(Vec<Thread>, Vec<DomainEvent>), CommandError> {
        let mut inner = self.lock();
        let moved = inner
            .threads
            .get(thread_id)
            .ok_or_else(|| CommandError::NotFound(format!("thread {thread_id} is not known")))?;
        if moved.deleted_at_ms.is_some() {
            return Err(CommandError::NotFound(format!(
                "thread {thread_id} is not known"
            )));
        }
        if moved.pinned_at_ms.is_none() || moved.pin_sort_key.is_none() {
            return Err(CommandError::Conflict(format!(
                "thread {thread_id} is not pinned"
            )));
        }

        let mut pinned: Vec<Thread> = inner
            .threads
            .values()
            .filter(|thread| {
                thread.deleted_at_ms.is_none()
                    && thread.pinned_at_ms.is_some()
                    && thread.pin_sort_key.is_some()
                    && thread.visibility == loom_domain::ThreadVisibility::Visible
            })
            .cloned()
            .collect();
        pinned.sort_by(|left, right| {
            left.pin_sort_key
                .cmp(&right.pin_sort_key)
                .then_with(|| left.id.cmp(&right.id))
        });
        if !pinned.iter().any(|thread| thread.id == *thread_id) {
            return Err(CommandError::Conflict(
                "the pinned thread is not visible in the current order".into(),
            ));
        }

        let neighbor = |id: Option<&ThreadId>| -> Result<Option<&Thread>, CommandError> {
            let Some(id) = id else {
                return Ok(None);
            };
            if id == thread_id {
                return Err(CommandError::Conflict(
                    "a pinned thread cannot be its own neighbor".into(),
                ));
            }
            pinned
                .iter()
                .find(|thread| thread.id == *id)
                .map(Some)
                .ok_or_else(|| CommandError::Conflict("pinned thread neighbor is stale".into()))
        };
        let previous = neighbor(previous_thread_id)?;
        let next = neighbor(next_thread_id)?;
        if previous.is_some_and(|thread| thread.pin_sort_key.is_none())
            || next.is_some_and(|thread| thread.pin_sort_key.is_none())
        {
            return Err(CommandError::Conflict(
                "pinned thread neighbor is stale".into(),
            ));
        }
        if previous
            .is_some_and(|left| next.is_some_and(|right| left.pin_sort_key >= right.pin_sort_key))
        {
            return Err(CommandError::Conflict(
                "the requested pinned neighbors are not ordered".into(),
            ));
        }

        let current_index = pinned
            .iter()
            .position(|thread| thread.id == *thread_id)
            .expect("the target was present in the pinned list");
        let current_previous = pinned
            .get(current_index.wrapping_sub(1))
            .map(|thread| &thread.id);
        let current_next = pinned.get(current_index + 1).map(|thread| &thread.id);
        if current_previous == previous_thread_id && current_next == next_thread_id {
            return Ok((pinned, Vec::new()));
        }

        let sort_key = match create_order_key_between(
            previous.and_then(|thread| thread.pin_sort_key.as_deref()),
            next.and_then(|thread| thread.pin_sort_key.as_deref()),
        ) {
            Ok(sort_key) => Some(sort_key),
            Err(error) if order_key_space_exhausted(&error) => None,
            Err(error) => return Err(error),
        };
        let mut events = Vec::new();
        if let Some(sort_key) = sort_key {
            let moved = inner
                .threads
                .get_mut(thread_id)
                .expect("the target was present in the registry");
            if let Some(event) = moved.set_pin(moved.pinned_at_ms, Some(sort_key), now_ms) {
                events.push(event);
            }
        } else {
            let mut desired_ids: Vec<ThreadId> = pinned
                .iter()
                .filter(|thread| thread.id != *thread_id)
                .map(|thread| thread.id.clone())
                .collect();
            let previous_index = previous_thread_id.and_then(|neighbor_id| {
                desired_ids
                    .iter()
                    .position(|candidate| candidate == neighbor_id)
            });
            let next_index = next_thread_id.and_then(|neighbor_id| {
                desired_ids
                    .iter()
                    .position(|candidate| candidate == neighbor_id)
            });
            let insert_index = match (previous_index, next_index) {
                (Some(index), _) => index + 1,
                (None, Some(index)) => index,
                (None, None) => 0,
            };
            desired_ids.insert(insert_index, thread_id.clone());
            let keys = rebalance_order_keys(desired_ids.len())?;
            for (id, key) in desired_ids.iter().zip(keys) {
                let pinned_thread = inner
                    .threads
                    .get_mut(id)
                    .expect("the pinned thread was collected");
                if let Some(event) =
                    pinned_thread.set_pin(pinned_thread.pinned_at_ms, Some(key), now_ms)
                {
                    events.push(event);
                }
            }
        }

        let mut reordered: Vec<Thread> = inner
            .threads
            .values()
            .filter(|thread| {
                thread.deleted_at_ms.is_none()
                    && thread.pinned_at_ms.is_some()
                    && thread.pin_sort_key.is_some()
                    && thread.visibility == loom_domain::ThreadVisibility::Visible
            })
            .cloned()
            .collect();
        reordered.sort_by(|left, right| {
            left.pin_sort_key
                .cmp(&right.pin_sort_key)
                .then_with(|| left.id.cmp(&right.id))
        });
        Ok((reordered, events))
    }

    /// Resolves mention ids in input order, omitting deleted or unknown rows.
    pub fn resolve_mention_threads(&self, thread_ids: &[ThreadId]) -> Vec<Thread> {
        let inner = self.lock();
        let mut resolved = Vec::new();
        for id in thread_ids {
            if resolved.iter().any(|thread: &Thread| thread.id == *id) {
                continue;
            }
            if let Some(thread) = inner
                .threads
                .get(id)
                .filter(|thread| thread.deleted_at_ms.is_none())
            {
                resolved.push(thread.clone());
            }
        }
        resolved
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

    /// Records the agent's identifier for a thread's conversation, and what it
    /// is bound to.
    ///
    /// Returns the event when the value changed, and `None` when it did not or
    /// the thread is gone — a report for a thread that no longer exists is not
    /// an error, because the domain has no way to have known it was deleted.
    pub fn set_provider_session_id(
        &self,
        thread_id: &ThreadId,
        session_id: &str,
        binding: Option<ProviderSessionBinding>,
        now_ms: u64,
    ) -> Option<DomainEvent> {
        let mut inner = self.lock();
        inner
            .threads
            .get_mut(thread_id)
            .and_then(|thread| thread.set_provider_session_id(session_id, binding, now_ms))
    }

    /// Names an untitled thread from the agent's own title for the
    /// conversation.
    ///
    /// Returns the event when the title changed, and `None` when it did not,
    /// the thread already has one, or the thread is gone — the same
    /// forgiveness as [`Self::set_provider_session_id`], because this is
    /// learned from a report rather than requested by a client.
    pub fn set_provider_title(
        &self,
        thread_id: &ThreadId,
        title: &str,
        now_ms: u64,
    ) -> Option<DomainEvent> {
        let mut inner = self.lock();
        inner
            .threads
            .get_mut(thread_id)
            .and_then(|thread| thread.set_provider_title(title, now_ms))
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

    /// Renames a host and returns the updated value.
    pub fn rename_host(
        &self,
        host_id: &HostId,
        name: String,
        now_ms: u64,
    ) -> Result<(Host, DomainEvent), CommandError> {
        let mut inner = self.lock();
        let host = inner
            .hosts
            .get_mut(host_id)
            .ok_or_else(|| CommandError::NotFound(format!("host {host_id} is not known")))?;
        host.rename(name, now_ms)?;
        let updated = host.clone();
        Ok((updated.clone(), DomainEvent::HostUpdated { host: updated }))
    }

    /// Updates the host's ACP permission ceiling.
    pub fn update_host_permission_ceiling(
        &self,
        host_id: &HostId,
        mode: loom_domain::HostPermissionMode,
        now_ms: u64,
    ) -> Result<(Host, DomainEvent), CommandError> {
        let mut inner = self.lock();
        let host = inner
            .hosts
            .get_mut(host_id)
            .ok_or_else(|| CommandError::NotFound(format!("host {host_id} is not known")))?;
        host.set_permission_ceiling(mode, now_ms);
        let updated = host.clone();
        Ok((updated.clone(), DomainEvent::HostUpdated { host: updated }))
    }

    /// Removes a host record after the caller has decided it is safe to do so.
    pub fn delete_host(&self, host_id: &HostId) -> Result<(Host, DomainEvent), CommandError> {
        let mut inner = self.lock();
        let host = inner
            .hosts
            .remove(host_id)
            .ok_or_else(|| CommandError::NotFound(format!("host {host_id} is not known")))?;
        let event = DomainEvent::HostDeleted {
            host_id: host.id.clone(),
            name: host.name.clone(),
        };
        Ok((host, event))
    }

    /// Returns whether at least one interaction is waiting for user attention.
    pub fn has_pending_interactions(&self) -> bool {
        !self
            .lock()
            .interactions
            .values()
            .all(|interaction| !interaction.status.is_open())
    }

    /// Returns a stable reason when a host is still referenced by domain state.
    pub fn host_reference(&self, host_id: &HostId) -> Option<&'static str> {
        let inner = self.lock();
        if inner.projects.values().any(|project| {
            project
                .sources
                .iter()
                .any(|source| &source.host_id == host_id)
        }) {
            return Some("project_source");
        }
        if inner
            .environments
            .values()
            .any(|environment| &environment.host_id == host_id)
        {
            return Some("environment");
        }
        None
    }

    /// Looks up a host.
    pub fn host(&self, host_id: &HostId) -> Option<Host> {
        self.lock().hosts.get(host_id).cloned()
    }

    // --- queued messages ----------------------------------------------------

    /// Queues a message for a thread, refusing an unknown thread.
    ///
    /// The thread is looked up first so the caller gets a `NotFound` rather
    /// than a row pointing at nothing: a queue entry with no thread can never
    /// be delivered, so it must not be created.
    pub fn create_queued_message(
        &self,
        new: NewQueuedMessage,
        now_ms: u64,
    ) -> Result<(QueuedMessage, DomainEvent), CommandError> {
        let mut inner = self.lock();
        let project_id = inner
            .threads
            .get(&new.thread_id)
            .filter(|thread| thread.deleted_at_ms.is_none())
            .map(|thread| thread.project_id.clone())
            .ok_or_else(|| {
                CommandError::NotFound(format!("thread {} is not known", new.thread_id))
            })?;
        if let Some(sender) = &new.sender_thread_id {
            if !inner.threads.contains_key(sender) {
                return Err(CommandError::NotFound(format!(
                    "sender thread {sender} is not known"
                )));
            }
        }
        let message = QueuedMessage::create(new, now_ms)?;
        let mut message = message;
        message.project_id = Some(project_id);
        let previous_key = inner
            .queued_messages
            .values()
            .filter(|existing| existing.thread_id == message.thread_id)
            .filter(|existing| !existing.sort_key.is_empty())
            .max_by(|left, right| {
                left.sort_key
                    .cmp(&right.sort_key)
                    .then_with(|| left.id.cmp(&right.id))
            })
            .map(|existing| existing.sort_key.as_str());
        message.sort_key = create_order_key_between(previous_key, None)?;
        inner
            .queued_messages
            .insert(message.id.clone(), message.clone());
        Ok((
            message.clone(),
            DomainEvent::ThreadQueuedMessageChanged {
                queued_message: message,
            },
        ))
    }

    /// Looks up a queued message.
    pub fn queued_message(&self, id: &QueuedMessageId) -> Option<QueuedMessage> {
        self.lock().queued_messages.get(id).cloned()
    }

    /// Every queued message, oldest first.
    pub fn queued_messages(&self) -> Vec<QueuedMessage> {
        self.queued_messages_for(None)
    }

    /// Every queued message for a thread, or for every thread when `thread_id`
    /// is `None`, oldest first.
    ///
    /// Oldest first is the drain order: the queue is a FIFO, and a client
    /// renders it top-down.
    pub fn queued_messages_for(&self, thread_id: Option<&ThreadId>) -> Vec<QueuedMessage> {
        let inner = self.lock();
        let mut messages: Vec<QueuedMessage> = inner
            .queued_messages
            .values()
            .filter(|message| {
                inner
                    .threads
                    .get(&message.thread_id)
                    .is_some_and(|thread| thread.deleted_at_ms.is_none())
            })
            .filter(|message| match thread_id {
                None => true,
                Some(id) => &message.thread_id == id,
            })
            .cloned()
            .collect();
        messages.sort_by(|left, right| {
            left.sort_key
                .cmp(&right.sort_key)
                .then_with(|| left.created_at_ms.cmp(&right.created_at_ms))
                .then_with(|| left.id.cmp(&right.id))
        });
        messages
    }

    /// Marks a queued message sent and returns the event.
    pub fn mark_queued_message_sent(
        &self,
        id: &QueuedMessageId,
        now_ms: u64,
    ) -> Result<(QueuedMessage, DomainEvent), CommandError> {
        let mut inner = self.lock();
        let message = inner
            .queued_messages
            .get_mut(id)
            .ok_or_else(|| CommandError::NotFound(format!("queued message {id} is not known")))?;
        message.mark_sent(now_ms)?;
        let message = message.clone();
        Ok((
            message.clone(),
            DomainEvent::ThreadQueuedMessageChanged {
                queued_message: message,
            },
        ))
    }

    /// Cancels a queued message and returns the event.
    pub fn cancel_queued_message(
        &self,
        id: &QueuedMessageId,
        now_ms: u64,
    ) -> Result<(QueuedMessage, DomainEvent), CommandError> {
        let mut inner = self.lock();
        let message = inner
            .queued_messages
            .get_mut(id)
            .ok_or_else(|| CommandError::NotFound(format!("queued message {id} is not known")))?;
        message.cancel(now_ms)?;
        let message = message.clone();
        Ok((
            message.clone(),
            DomainEvent::ThreadQueuedMessageChanged {
                queued_message: message,
            },
        ))
    }

    /// Records why a queued message could not be sent, leaving it queued.
    pub fn record_queued_message_failure(
        &self,
        id: &QueuedMessageId,
        reason: String,
        now_ms: u64,
    ) -> Result<(QueuedMessage, DomainEvent), CommandError> {
        let mut inner = self.lock();
        let message = inner
            .queued_messages
            .get_mut(id)
            .ok_or_else(|| CommandError::NotFound(format!("queued message {id} is not known")))?;
        message.record_failure(reason, now_ms);
        let message = message.clone();
        Ok((
            message.clone(),
            DomainEvent::ThreadQueuedMessageChanged {
                queued_message: message,
            },
        ))
    }

    /// Clears a recorded send failure so a queued message is deliverable again.
    pub fn clear_queued_message_failure(
        &self,
        id: &QueuedMessageId,
        now_ms: u64,
    ) -> Result<(QueuedMessage, DomainEvent), CommandError> {
        let mut inner = self.lock();
        let message = inner
            .queued_messages
            .get_mut(id)
            .ok_or_else(|| CommandError::NotFound(format!("queued message {id} is not known")))?;
        message.clear_failure(now_ms);
        let message = message.clone();
        Ok((
            message.clone(),
            DomainEvent::ThreadQueuedMessageChanged {
                queued_message: message,
            },
        ))
    }

    /// Replaces a queued prompt under a compare-and-swap timestamp.
    pub fn update_queued_message(
        &self,
        thread_id: &ThreadId,
        id: &QueuedMessageId,
        expected_updated_at_ms: u64,
        text: String,
        now_ms: u64,
    ) -> Result<(QueuedMessage, DomainEvent), CommandError> {
        let mut inner = self.lock();
        if !inner
            .threads
            .get(thread_id)
            .is_some_and(|thread| thread.deleted_at_ms.is_none())
        {
            return Err(CommandError::NotFound(format!(
                "thread {thread_id} is not known"
            )));
        }
        let message = inner
            .queued_messages
            .get_mut(id)
            .ok_or_else(|| CommandError::NotFound(format!("queued message {id} is not known")))?;
        if message.thread_id != *thread_id {
            return Err(CommandError::NotFound(format!(
                "queued message {id} belongs to another thread"
            )));
        }
        if message.updated_at_ms != expected_updated_at_ms {
            return Err(CommandError::Conflict(format!(
                "queued message {id} was updated at {}, not the expected {}",
                message.updated_at_ms, expected_updated_at_ms
            )));
        }
        message.update_text(text, now_ms)?;
        let message = message.clone();
        Ok((
            message.clone(),
            DomainEvent::ThreadQueuedMessageChanged {
                queued_message: message,
            },
        ))
    }

    /// Reorders one open queued message between two optional neighbors.
    ///
    /// The returned rows are the current open queue, and the returned events
    /// contain every entity mutation made by the command. `group_boundary_id`
    /// is accepted as the caller's grouping anchor; grouping itself is changed
    /// only by `set_queued_message_group_boundary`, so moving a row does not
    /// silently rewrite its grouping edges.
    pub fn reorder_queued_message(
        &self,
        thread_id: &ThreadId,
        id: &QueuedMessageId,
        previous_id: Option<&QueuedMessageId>,
        next_id: Option<&QueuedMessageId>,
        group_boundary_id: Option<&QueuedMessageId>,
        now_ms: u64,
    ) -> Result<(Vec<QueuedMessage>, Vec<DomainEvent>), CommandError> {
        let mut inner = self.lock();
        if !inner
            .threads
            .get(thread_id)
            .is_some_and(|thread| thread.deleted_at_ms.is_none())
        {
            return Err(CommandError::NotFound(format!(
                "thread {thread_id} is not known"
            )));
        }

        let target =
            inner.queued_messages.get(id).cloned().ok_or_else(|| {
                CommandError::NotFound(format!("queued message {id} is not known"))
            })?;
        if target.thread_id != *thread_id {
            return Err(CommandError::NotFound(format!(
                "queued message {id} belongs to another thread"
            )));
        }
        if !target.status.is_open() {
            return Err(CommandError::Conflict(format!(
                "queued message {id} is already {}",
                target.status
            )));
        }

        let mut queued: Vec<QueuedMessage> = inner
            .queued_messages
            .values()
            .filter(|message| {
                message.thread_id == *thread_id && message.status == QueuedMessageStatus::Queued
            })
            .cloned()
            .collect();
        queued.sort_by(|left, right| {
            left.sort_key
                .cmp(&right.sort_key)
                .then_with(|| left.created_at_ms.cmp(&right.created_at_ms))
                .then_with(|| left.id.cmp(&right.id))
        });

        let neighbor = |neighbor_id: Option<&QueuedMessageId>| {
            let Some(neighbor_id) = neighbor_id else {
                return Ok(None);
            };
            if neighbor_id == id {
                return Err(CommandError::Conflict(
                    "a queued message cannot be its own neighbor".into(),
                ));
            }
            queued
                .iter()
                .find(|message| message.id == *neighbor_id)
                .map(Some)
                .ok_or_else(|| CommandError::Conflict("queued message neighbor is stale".into()))
        };
        let previous = neighbor(previous_id)?;
        let next = neighbor(next_id)?;
        let grouping_anchor = neighbor(group_boundary_id)?;
        if let Some(anchor) = grouping_anchor {
            if !anchor.group_with_next && anchor.id != target.id {
                return Err(CommandError::Conflict(
                    "group boundary anchor is not part of a grouped prefix".into(),
                ));
            }
        }
        let current_index = queued
            .iter()
            .position(|message| message.id == *id)
            .expect("the target was present in the queued list");
        let current_previous = current_index
            .checked_sub(1)
            .and_then(|index| queued.get(index))
            .map(|message| &message.id);
        let current_next = queued.get(current_index + 1).map(|message| &message.id);
        if current_previous == previous_id && current_next == next_id {
            return Ok((queued, Vec::new()));
        }

        // Build the intended order without the target first. This validates
        // that `previous` really precedes `next`, even when the caller moves a
        // row across both of its old neighbors.
        let mut desired_ids: Vec<QueuedMessageId> = queued
            .iter()
            .filter(|message| message.id != *id)
            .map(|message| message.id.clone())
            .collect();
        let previous_index = previous_id.and_then(|neighbor_id| {
            desired_ids
                .iter()
                .position(|candidate| candidate == neighbor_id)
        });
        let next_index = next_id.and_then(|neighbor_id| {
            desired_ids
                .iter()
                .position(|candidate| candidate == neighbor_id)
        });
        if (previous_id.is_some() && previous_index.is_none())
            || (next_id.is_some() && next_index.is_none())
        {
            return Err(CommandError::Conflict(
                "queued message neighbor is stale".into(),
            ));
        }
        if let (Some(previous_index), Some(next_index)) = (previous_index, next_index) {
            if previous_index >= next_index {
                return Err(CommandError::Conflict(
                    "the requested queued neighbors are not ordered".into(),
                ));
            }
        }
        let insert_index = match (previous_index, next_index) {
            (Some(index), _) => index + 1,
            (None, Some(index)) => index,
            (None, None) => 0,
        };
        desired_ids.insert(insert_index, id.clone());

        let mut events = Vec::new();
        let sort_key = match create_order_key_between(
            previous.map(|message| message.sort_key.as_str()),
            next.map(|message| message.sort_key.as_str()),
        ) {
            Ok(sort_key)
                if order_key_is_occupied(
                    &sort_key,
                    queued
                        .iter()
                        .filter(|message| message.id != *id)
                        .map(|message| message.sort_key.as_str()),
                ) =>
            {
                Err(CommandError::Conflict(ORDER_KEY_SPACE_EXHAUSTED.into()))
            }
            result => result,
        };
        match sort_key {
            Ok(sort_key) => {
                let message = inner
                    .queued_messages
                    .get_mut(id)
                    .expect("the target was present in the registry");
                message.set_sort_key(sort_key, now_ms);
                events.push(DomainEvent::ThreadQueuedMessageChanged {
                    queued_message: message.clone(),
                });
            }
            Err(error) if order_key_space_exhausted(&error) => {
                // Adjacent-prefix keys can be genuinely consecutive in the
                // lexicographic order (`0` and `00`). Rebalance the entire
                // desired order atomically and publish every changed row so a
                // replaying client cannot observe a locally invented order.
                let keys = rebalance_order_keys(desired_ids.len())?;
                for (message_id, key) in desired_ids.iter().zip(keys) {
                    let message = inner
                        .queued_messages
                        .get_mut(message_id)
                        .expect("the queued message was collected");
                    if message.sort_key == key {
                        continue;
                    }
                    message.set_sort_key(key, now_ms);
                    events.push(DomainEvent::ThreadQueuedMessageChanged {
                        queued_message: message.clone(),
                    });
                }
            }
            Err(error) => return Err(error),
        }

        let mut reordered: Vec<QueuedMessage> = inner
            .queued_messages
            .values()
            .filter(|message| {
                message.thread_id == *thread_id && message.status == QueuedMessageStatus::Queued
            })
            .cloned()
            .collect();
        reordered.sort_by(|left, right| {
            left.sort_key
                .cmp(&right.sort_key)
                .then_with(|| left.created_at_ms.cmp(&right.created_at_ms))
                .then_with(|| left.id.cmp(&right.id))
        });
        Ok((reordered, events))
    }

    /// Sets the grouping boundary after an optimistic prefix check.
    ///
    /// The prefix names the contiguous rows the caller believes are grouped;
    /// the boundary row is the first row after that prefix. The edge immediately
    /// before the boundary is kept open, and the boundary's own edge is closed.
    /// A prefix mismatch is a conflict rather than a best-effort rewrite.
    pub fn set_queued_message_group_boundary(
        &self,
        thread_id: &ThreadId,
        expected_grouped_prefix_ids: &[QueuedMessageId],
        boundary_id: &QueuedMessageId,
        now_ms: u64,
    ) -> Result<(Vec<QueuedMessage>, Vec<DomainEvent>), CommandError> {
        let mut inner = self.lock();
        if !inner
            .threads
            .get(thread_id)
            .is_some_and(|thread| thread.deleted_at_ms.is_none())
        {
            return Err(CommandError::NotFound(format!(
                "thread {thread_id} is not known"
            )));
        }

        let mut queued: Vec<QueuedMessage> = inner
            .queued_messages
            .values()
            .filter(|message| {
                message.thread_id == *thread_id && message.status == QueuedMessageStatus::Queued
            })
            .cloned()
            .collect();
        queued.sort_by(|left, right| {
            left.sort_key
                .cmp(&right.sort_key)
                .then_with(|| left.created_at_ms.cmp(&right.created_at_ms))
                .then_with(|| left.id.cmp(&right.id))
        });

        let boundary_index = queued
            .iter()
            .position(|message| message.id == *boundary_id)
            .ok_or_else(|| {
                CommandError::NotFound(format!("queued message {boundary_id} is not known"))
            })?;
        if expected_grouped_prefix_ids.is_empty()
            || expected_grouped_prefix_ids.len() != boundary_index
            || queued
                .iter()
                .take(boundary_index)
                .map(|message| &message.id)
                .ne(expected_grouped_prefix_ids.iter())
        {
            return Err(CommandError::Conflict(
                "the queued message grouping changed; reload before setting its boundary".into(),
            ));
        }

        let mut events = Vec::new();
        for (index, message) in queued.iter().enumerate() {
            let should_group = index + 1 < boundary_index;
            if message.group_with_next == should_group {
                continue;
            }
            let stored = inner
                .queued_messages
                .get_mut(&message.id)
                .expect("the queued message was collected");
            stored.set_group_with_next(should_group, now_ms);
            events.push(DomainEvent::ThreadQueuedMessageChanged {
                queued_message: stored.clone(),
            });
        }

        let mut result: Vec<QueuedMessage> = inner
            .queued_messages
            .values()
            .filter(|message| {
                message.thread_id == *thread_id && message.status == QueuedMessageStatus::Queued
            })
            .cloned()
            .collect();
        result.sort_by(|left, right| {
            left.sort_key
                .cmp(&right.sort_key)
                .then_with(|| left.created_at_ms.cmp(&right.created_at_ms))
                .then_with(|| left.id.cmp(&right.id))
        });
        Ok((result, events))
    }

    // --- interactions -------------------------------------------------------

    /// Records an interaction for a thread, refusing an unknown thread.
    ///
    /// A provider that repeats its request id gets the same row back when the
    /// caller passes a deterministic [`InteractionId`]: the existing
    /// interaction is returned unchanged rather than duplicated, which is what
    /// makes a redelivered provider frame idempotent.
    pub fn create_interaction(
        &self,
        new: NewInteraction,
        now_ms: u64,
    ) -> Result<(Interaction, Option<DomainEvent>), CommandError> {
        let mut inner = self.lock();
        let project_id = inner
            .threads
            .get(&new.thread_id)
            .map(|thread| thread.project_id.clone())
            .ok_or_else(|| {
                CommandError::NotFound(format!("thread {} is not known", new.thread_id))
            })?;
        if let Some(id) = &new.id {
            if let Some(existing) = inner.interactions.get(id) {
                return Ok((existing.clone(), None));
            }
        }
        let mut interaction = Interaction::create(new, now_ms)?;
        interaction.project_id = Some(project_id);
        inner
            .interactions
            .insert(interaction.id.clone(), interaction.clone());
        Ok((
            interaction.clone(),
            Some(DomainEvent::ThreadInteractionChanged { interaction }),
        ))
    }

    /// Looks up an interaction.
    pub fn interaction(&self, id: &InteractionId) -> Option<Interaction> {
        self.lock().interactions.get(id).cloned()
    }

    /// Every interaction, oldest first.
    pub fn interactions(&self) -> Vec<Interaction> {
        self.interactions_for(None)
    }

    /// Every interaction for a thread, or for every thread when `thread_id` is
    /// `None`, oldest first.
    pub fn interactions_for(&self, thread_id: Option<&ThreadId>) -> Vec<Interaction> {
        let mut interactions: Vec<Interaction> = self
            .lock()
            .interactions
            .values()
            .filter(|interaction| match thread_id {
                None => true,
                Some(id) => &interaction.thread_id == id,
            })
            .cloned()
            .collect();
        interactions.sort_by(|left, right| {
            left.created_at_ms
                .cmp(&right.created_at_ms)
                .then_with(|| left.id.cmp(&right.id))
        });
        interactions
    }

    /// The interactions of a thread that have not reached a terminal state.
    pub fn pending_interactions(&self, thread_id: &ThreadId) -> Vec<Interaction> {
        self.interactions_for(Some(thread_id))
            .into_iter()
            .filter(|interaction| !interaction.status.is_terminal())
            .collect()
    }

    /// Accepts an answer and records the replayable delivery intent.
    pub fn prepare_interaction_resolution(
        &self,
        id: &InteractionId,
        resolution: Resolution,
        now_ms: u64,
    ) -> Result<(Interaction, DomainEvent), CommandError> {
        let mut inner = self.lock();
        let interaction = inner
            .interactions
            .get_mut(id)
            .ok_or_else(|| CommandError::NotFound(format!("interaction {id} is not known")))?;
        interaction.begin_resolution(resolution, now_ms)?;
        let interaction = interaction.clone();
        Ok((
            interaction.clone(),
            DomainEvent::ThreadInteractionChanged { interaction },
        ))
    }

    /// Completes an answer after its provider delivery was stored.
    pub fn complete_interaction_resolution(
        &self,
        id: &InteractionId,
        now_ms: u64,
    ) -> Result<(Interaction, DomainEvent), CommandError> {
        let mut inner = self.lock();
        let interaction = inner
            .interactions
            .get_mut(id)
            .ok_or_else(|| CommandError::NotFound(format!("interaction {id} is not known")))?;
        interaction.complete_resolution(now_ms)?;
        let interaction = interaction.clone();
        Ok((
            interaction.clone(),
            DomainEvent::ThreadInteractionChanged { interaction },
        ))
    }

    /// Answers an interaction and returns the event to publish.
    ///
    /// A resolution whose kind does not match the payload is a
    /// [`CommandError::Domain`] (the domain's own `InvalidField`), so the route
    /// reports `400 invalid_request` rather than a conflict: the request was
    /// well-formed, it named the wrong operation for this interaction.
    pub fn resolve_interaction(
        &self,
        id: &InteractionId,
        resolution: Resolution,
        now_ms: u64,
    ) -> Result<(Interaction, DomainEvent), CommandError> {
        let mut inner = self.lock();
        let interaction = inner
            .interactions
            .get_mut(id)
            .ok_or_else(|| CommandError::NotFound(format!("interaction {id} is not known")))?;
        if !interaction.status.is_open() {
            return Err(CommandError::Conflict(format!(
                "interaction {id} is already {}",
                interaction.status
            )));
        }
        interaction.resolve(resolution, now_ms)?;
        let interaction = interaction.clone();
        Ok((
            interaction.clone(),
            DomainEvent::ThreadInteractionChanged { interaction },
        ))
    }

    /// Records a cancellation that still has to reach a blocked provider.
    pub fn prepare_interaction_cancellation(
        &self,
        id: &InteractionId,
        reason: Option<String>,
        now_ms: u64,
    ) -> Result<(Interaction, DomainEvent), CommandError> {
        let mut inner = self.lock();
        let interaction = inner
            .interactions
            .get_mut(id)
            .ok_or_else(|| CommandError::NotFound(format!("interaction {id} is not known")))?;
        interaction.begin_cancellation(reason, now_ms)?;
        let interaction = interaction.clone();
        Ok((
            interaction.clone(),
            DomainEvent::ThreadInteractionChanged { interaction },
        ))
    }

    /// Completes a cancellation after its provider delivery was stored.
    pub fn complete_interaction_cancellation(
        &self,
        id: &InteractionId,
        now_ms: u64,
    ) -> Result<(Interaction, DomainEvent), CommandError> {
        let mut inner = self.lock();
        let interaction = inner
            .interactions
            .get_mut(id)
            .ok_or_else(|| CommandError::NotFound(format!("interaction {id} is not known")))?;
        interaction.complete_cancellation(now_ms)?;
        let interaction = interaction.clone();
        Ok((
            interaction.clone(),
            DomainEvent::ThreadInteractionChanged { interaction },
        ))
    }

    /// Settles an interaction without an answer.
    ///
    /// Cancelling an already-terminal interaction is a conflict, matching
    /// [`DomainRegistry::resolve_interaction`]: a client that cancels twice is
    /// racing someone else, and the second cancel must not look like success.
    pub fn cancel_interaction(
        &self,
        id: &InteractionId,
        reason: Option<String>,
        now_ms: u64,
    ) -> Result<(Interaction, DomainEvent), CommandError> {
        let mut inner = self.lock();
        let interaction = inner
            .interactions
            .get_mut(id)
            .ok_or_else(|| CommandError::NotFound(format!("interaction {id} is not known")))?;
        if interaction.status.is_terminal() {
            return Err(CommandError::Conflict(format!(
                "interaction {id} is already {}",
                interaction.status
            )));
        }
        interaction.cancel(reason, now_ms)?;
        let interaction = interaction.clone();
        Ok((
            interaction.clone(),
            DomainEvent::ThreadInteractionChanged { interaction },
        ))
    }

    /// Every open interaction a thread has, for the sidebar's activity flag.
    pub fn has_pending_interaction(&self, thread_id: &ThreadId) -> bool {
        !self.pending_interactions(thread_id).is_empty()
    }

    /// How many queued messages a thread still has to send.
    pub fn queued_message_count(&self, thread_id: &ThreadId) -> usize {
        self.lock()
            .queued_messages
            .values()
            .filter(|message| {
                &message.thread_id == thread_id && message.status == QueuedMessageStatus::Queued
            })
            .count()
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
        let mut queued_messages: Vec<QueuedMessage> =
            inner.queued_messages.values().cloned().collect();
        queued_messages.sort_by(|left, right| left.id.cmp(&right.id));
        let mut interactions: Vec<Interaction> = inner.interactions.values().cloned().collect();
        interactions.sort_by(|left, right| left.id.cmp(&right.id));
        let mut thread_sections: Vec<ThreadSection> =
            inner.thread_sections.values().cloned().collect();
        thread_sections.sort_by(|left, right| left.id.cmp(&right.id));
        RegistrySnapshot {
            personal_project_id: inner.personal_project_id.clone(),
            projects,
            threads,
            hosts,
            environments,
            queued_messages,
            interactions,
            thread_sections,
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
            // The event carries the thread after the change, so replay is a
            // whole-value overwrite; a thread the registry never saw (an event
            // from a snapshot the process has not loaded) stays unknown.
            DomainEvent::ThreadUpdated { thread: updated } => {
                if let Some(thread) = inner.threads.get_mut(&updated.id) {
                    *thread = updated.clone();
                }
            }
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
            DomainEvent::ThreadQueuedMessageChanged { queued_message } => {
                inner
                    .queued_messages
                    .insert(queued_message.id.clone(), queued_message.clone());
            }
            DomainEvent::ThreadInteractionChanged { interaction } => {
                inner
                    .interactions
                    .insert(interaction.id.clone(), interaction.clone());
            }
            DomainEvent::HostRegistered { host } => {
                inner.hosts.insert(host.id.clone(), host.clone());
            }
            DomainEvent::ThreadSectionCreated { section }
            | DomainEvent::ThreadSectionUpdated { section } => {
                inner
                    .thread_sections
                    .insert(section.id.clone(), section.clone());
            }
            DomainEvent::ThreadSectionDeleted { section_id, .. } => {
                inner.thread_sections.remove(section_id);
            }
            DomainEvent::HostStatusChanged {
                host_id, to, at_ms, ..
            } => {
                if let Some(host) = inner.hosts.get_mut(host_id) {
                    host.status = *to;
                    host.updated_at_ms = *at_ms;
                }
            }
            DomainEvent::HostUpdated { host } => {
                inner.hosts.insert(host.id.clone(), host.clone());
            }
            DomainEvent::HostDeleted { host_id, .. } => {
                inner.hosts.remove(host_id);
            }
            DomainEvent::EnvironmentCreated { environment } => {
                inner
                    .environments
                    .insert(environment.id.clone(), environment.clone());
            }
            DomainEvent::EnvironmentUpdated { environment } => {
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
        let mut queued_messages = snapshot
            .queued_messages
            .into_iter()
            .map(|message| (message.id.clone(), message))
            .collect();
        let interactions = snapshot
            .interactions
            .into_iter()
            .map(|interaction| (interaction.id.clone(), interaction))
            .collect();
        let thread_sections = snapshot
            .thread_sections
            .into_iter()
            .map(|section| (section.id.clone(), section))
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
                deleted_at_ms: None,
                sort_key: None,
                created_at_ms: 0,
                updated_at_ms: 0,
            });
        // B4 introduced durable fractional queue ordering. Migrate legacy
        // rows while restoring the baseline, where no client can observe an
        // unannounced mutation and the next snapshot persists the keys.
        normalize_queued_order_keys(&mut queued_messages);
        Self {
            personal_project_id,
            projects,
            threads,
            hosts,
            environments,
            queued_messages,
            interactions,
            thread_sections,
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
    fn a_provider_session_id_is_stored_and_replayed() {
        let registry = registry();
        let (thread, _) = registry
            .create_thread(Some(personal(&registry)), Some("acp".into()), None, 2)
            .unwrap();

        let host = HostId::mint();
        let binding = ProviderSessionBinding::new("pi", "/srv/project-a")
            .on_host(host.clone())
            .at(3);
        let event = registry
            .set_provider_session_id(&thread.id, "acp-session-1", Some(binding.clone()), 3)
            .expect("the first ACP identity changes the thread");
        assert_eq!(
            registry
                .thread(&thread.id)
                .unwrap()
                .provider_session_id
                .as_deref(),
            Some("acp-session-1")
        );

        let restored = DomainRegistry::new(1);
        restored.apply_event(&DomainEvent::ThreadCreated { thread });
        restored.apply_event(&event);
        let DomainEvent::ThreadUpdated { thread: updated } = &event else {
            panic!("expected a thread update");
        };
        assert_eq!(
            restored
                .thread(&updated.id)
                .and_then(|thread| thread.provider_session_id),
            Some("acp-session-1".to_owned())
        );
        // The binding survives the round trip, which is the whole point of
        // recording it: it decides whether a later run may resume.
        let restored_thread = restored.thread(&updated.id).unwrap();
        assert_eq!(
            restored_thread.provider_session_binding.as_ref(),
            Some(&binding)
        );
        assert!(restored_thread.may_resume_session("pi", "/srv/project-a", &host));
        assert!(!restored_thread.may_resume_session("other-agent", "/srv/project-a", &host));
        assert!(!restored_thread.may_resume_session("pi", "/srv/elsewhere", &host));
        assert!(!restored_thread.may_resume_session("pi", "/srv/project-a", &HostId::mint()));
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

        // The worker reconnects with the id it was given: no second host, no
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
    fn a_changed_data_directory_is_replayed_with_the_reconnect() {
        let registry = registry();
        let (host, _) = registry
            .enroll_host_with_data_dir(None, "laptop".into(), Some("/var/lib/loom".into()), 2)
            .unwrap();
        registry.mark_host_disconnected(&host.id, 3).unwrap();

        let (reconnected, events) = registry
            .enroll_host_with_data_dir(
                Some(host.id.clone()),
                "laptop".into(),
                Some("/srv/loom".into()),
                4,
            )
            .unwrap();
        assert_eq!(reconnected.data_dir.as_deref(), Some("/srv/loom"));
        assert_eq!(events.len(), 2);
        match &events[0] {
            DomainEvent::HostUpdated { host } => {
                assert_eq!(host.data_dir.as_deref(), Some("/srv/loom"));
                assert_eq!(host.status, loom_domain::HostStatus::Connected);
            }
            other => panic!("expected a host update, got {other:?}"),
        }
        assert!(matches!(
            events[1],
            DomainEvent::HostStatusChanged {
                from: loom_domain::HostStatus::Disconnected,
                to: loom_domain::HostStatus::Connected,
                ..
            }
        ));
    }

    #[test]
    fn a_server_with_no_worker_has_no_primary_host_but_no_error() {
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
