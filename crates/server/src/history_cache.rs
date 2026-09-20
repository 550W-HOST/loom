//! The in-memory timeline cache: a displayable conversation, rebuildable.
//!
//! A thread's history lives with the agent that owns its ACP session, so the
//! server holds no copy of it that it could call durable. What it keeps is a
//! **cache**: per thread, a baseline loaded from the owning host plus whatever
//! has happened since, keyed by a `generation` the client echoes back so a page
//! fetched before a rebuild can never be stitched onto one fetched after.
//!
//! Two rules make the cache safe to reason about, and both are about identity
//! rather than content:
//!
//! * **A sequence is assigned when a row is installed or appended, and never
//!   recomputed.** Deriving it from a row's position in a list is what made
//!   `sourceSeq` saturate once the relay's shard filled up; a stored counter
//!   cannot do that, and trimming rows does not renumber the survivors.
//! * **A generation is never reused.** Rebuilding a baseline, evicting a thread
//!   and reloading it, and a binding change all mint a new one, so a cursor
//!   from an older generation is recognised as stale instead of being read as a
//!   position in the new one.
//!
//! Nothing here talks to a host. [`HistoryCache::begin_load`] is the gate that
//! makes one caller the loader and the rest waiters; the caller that wins does
//! the load and installs the result.

use std::collections::HashMap;
use std::sync::Mutex;

use loom_domain::{HostId, ProviderEvent, ThreadId};

/// What a cached conversation is bound to.
///
/// Every part matters: the session id names a file on one host's disk, issued
/// by one agent, meaningful for one directory. A cache entry whose binding no
/// longer matches the thread is not a stale view of the same conversation — it
/// is a different conversation, and using it would show one thread's history
/// under another's name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CacheBinding {
    /// The machine that owns the session.
    pub host_id: HostId,
    /// The agent that issued the session id.
    pub agent: String,
    /// The agent's own session id.
    pub provider_session_id: String,
    /// The workspace the session was opened in.
    pub cwd: String,
}

/// How much of a conversation the cache can currently offer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HistoryStatus {
    /// A baseline is being loaded and there is none to show yet.
    Loading,
    /// Some of the conversation is here and it is not all of it: the live
    /// overlay of a thread whose history has not been loaded yet. Showing it is
    /// better than showing nothing, and calling it complete would be a lie.
    Partial,
    /// A complete baseline is installed.
    Ready,
    /// Something is cached but a newer view is pending.
    Stale,
    /// There is nothing to show, and `reason` says why.
    Unavailable,
}

/// One cached row: a stable sequence and the provider event behind it.
#[derive(Clone, Debug, PartialEq)]
pub struct CachedRow {
    /// Stable within the entry's generation. Assigned once, never recomputed.
    pub seq: u64,
    /// The contract event a projection turns into a timeline row.
    pub event: ProviderEvent,
}

/// What a reader gets: a whole conversation and the generation it belongs to.
#[derive(Clone, Debug, PartialEq)]
pub struct CacheView {
    /// The generation every `seq` in `rows` belongs to.
    pub generation: u64,
    /// How much the cache can offer.
    pub status: HistoryStatus,
    /// Whether the rows are a complete conversation. A `Ready` entry with no
    /// rows is a real empty session; an incomplete one is not empty, it is
    /// partial, and saying so is the difference the caller must not lose.
    pub complete: bool,
    /// Why the conversation is not fully available, when it is not.
    pub reason: Option<String>,
    /// The rows, oldest first.
    pub rows: Vec<CachedRow>,
}

/// Whether this caller should perform the load it asked about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoadTicket {
    /// No load was in flight for this thread and binding: this caller owns it.
    Leader,
    /// Another caller is already loading it; wait for their result.
    Follower,
    /// Too many loads are in flight, so this one was refused rather than
    /// queued. Refusing keeps the memory bound honest; a queue would hide it.
    Refused,
}

struct Entry {
    generation: u64,
    /// What this conversation is bound to, when a session is known.
    ///
    /// `None` is a thread the agent has not reported a session for yet: it can
    /// still show what the user has said, but nothing has confirmed it is the
    /// whole conversation.
    binding: Option<CacheBinding>,
    status: HistoryStatus,
    reason: Option<String>,
    rows: Vec<CachedRow>,
    /// The sequence the next appended row gets.
    next_seq: u64,
    /// Bytes of the serialized rows, for the budget.
    bytes: u64,
    /// Monotone access counter, so least-recently-used is exact and testable.
    last_used: u64,
}

struct Inner {
    entries: HashMap<ThreadId, Entry>,
    bytes: u64,
    next_generation: u64,
    next_tick: u64,
    /// Loads in flight, keyed by thread. A second caller for the same thread
    /// waits rather than loading the same conversation twice.
    loads: HashMap<ThreadId, CacheBinding>,
}

/// A bounded, per-thread cache of displayable conversations.
pub struct HistoryCache {
    inner: Mutex<Inner>,
    max_threads: usize,
    max_total_bytes: u64,
    max_concurrent_loads: usize,
}

impl HistoryCache {
    /// Creates a cache holding at most `max_threads` conversations and
    /// `max_total_bytes` of serialized rows.
    pub fn new(max_threads: usize, max_total_bytes: u64, max_concurrent_loads: usize) -> Self {
        Self {
            inner: Mutex::new(Inner {
                entries: HashMap::new(),
                bytes: 0,
                next_generation: 1,
                next_tick: 0,
                loads: HashMap::new(),
            }),
            max_threads: max_threads.max(1),
            max_total_bytes: max_total_bytes.max(1),
            max_concurrent_loads: max_concurrent_loads.max(1),
        }
    }

    /// Claims the right to load `thread_id` under `binding`.
    ///
    /// Exactly one caller per thread is the leader; the others are told to
    /// wait. A load for a thread whose binding changed while an older load was
    /// still running is a different load, so it is admitted rather than
    /// mistaken for the one in flight.
    pub fn begin_load(&self, thread_id: &ThreadId, binding: &CacheBinding) -> LoadTicket {
        let mut inner = self.lock();
        if inner.loads.get(thread_id) == Some(binding) {
            return LoadTicket::Follower;
        }
        if inner.loads.len() >= self.max_concurrent_loads {
            return LoadTicket::Refused;
        }
        inner.loads.insert(thread_id.clone(), binding.clone());
        LoadTicket::Leader
    }

    /// Releases a load claim, whatever its outcome.
    pub fn finish_load(&self, thread_id: &ThreadId) {
        let mut inner = self.lock();
        inner.loads.remove(thread_id);
        // Touch so a thread that just failed to load is not the first
        // candidate for eviction: it was used most recently.
        inner.next_tick += 1;
        let tick = inner.next_tick;
        if let Some(entry) = inner.entries.get_mut(thread_id) {
            entry.last_used = tick;
        }
    }

    /// Replaces a thread's baseline with a whole conversation from `binding`.
    ///
    /// The rows are numbered from one under a **new generation**, so a client
    /// holding a cursor from the previous one is told to refetch rather than
    /// reading its cursor as a position in this conversation.
    pub fn install_baseline(
        &self,
        thread_id: &ThreadId,
        binding: CacheBinding,
        events: Vec<ProviderEvent>,
    ) -> u64 {
        let mut inner = self.lock();
        let generation = inner.take_generation();
        let mut rows = Vec::with_capacity(events.len());
        let mut bytes = 0u64;
        let mut seq = 0u64;
        for event in events {
            seq += 1;
            bytes = bytes.saturating_add(row_bytes(&event));
            rows.push(CachedRow { seq, event });
        }
        inner.remove(thread_id);
        inner.insert(
            thread_id.clone(),
            Entry {
                generation,
                binding: Some(binding),
                status: HistoryStatus::Ready,
                reason: None,
                rows,
                next_seq: seq + 1,
                bytes,
                last_used: 0,
            },
        );
        inner.evict_to_fit(self.max_threads, self.max_total_bytes);
        generation
    }

    /// Appends one live event to a thread's cached conversation.
    ///
    /// Returns the sequence it was given, or `None` when the event was
    /// refused. A refusal is the safe answer for a mismatched binding: this
    /// event belongs to a conversation the cache is not holding.
    ///
    /// A thread with no entry yet gets one. That is the thread the agent has
    /// not reported a session for — a brand-new conversation, or one whose
    /// first run is still in flight — and what the user has already said
    /// belongs on screen. It is marked partial, because nothing has confirmed
    /// it is the whole conversation, and the next load replaces it with the
    /// baseline the agent replays.
    pub fn append_live(
        &self,
        thread_id: &ThreadId,
        binding: Option<&CacheBinding>,
        event: ProviderEvent,
    ) -> Option<u64> {
        let mut inner = self.lock();
        inner.next_tick += 1;
        let tick = inner.next_tick;
        let size = row_bytes(&event);
        let binding = binding.cloned();

        if !inner.entries.contains_key(thread_id) {
            let generation = inner.take_generation();
            inner.insert(
                thread_id.clone(),
                Entry {
                    generation,
                    binding: binding.clone(),
                    status: HistoryStatus::Partial,
                    reason: Some("the conversation's history has not been loaded yet".to_owned()),
                    rows: Vec::new(),
                    next_seq: 1,
                    bytes: 0,
                    last_used: tick,
                },
            );
        }

        let seq = {
            let entry = inner
                .entries
                .get_mut(thread_id)
                .expect("the entry exists or was just created");
            match (entry.binding.as_ref(), binding.as_ref()) {
                // A different conversation: this event must not land in this
                // one.
                (Some(known), Some(offered)) if known != offered => return None,
                // The first event that names a session adopts it.
                (None, Some(offered)) => entry.binding = Some(offered.clone()),
                _ => {}
            }
            let seq = entry.next_seq;
            entry.next_seq += 1;
            entry.bytes = entry.bytes.saturating_add(size);
            entry.rows.push(CachedRow { seq, event });
            entry.last_used = tick;
            seq
        };

        inner.bytes = inner.bytes.saturating_add(size);
        if inner.bytes > self.max_total_bytes || inner.entries.len() > self.max_threads {
            inner.evict_to_fit(self.max_threads, self.max_total_bytes);
        }
        Some(seq)
    }

    /// Records that a live overlay is no longer current for a thread.
    pub fn mark_stale(&self, thread_id: &ThreadId, reason: impl Into<String>) {
        let mut inner = self.lock();
        if let Some(entry) = inner.entries.get_mut(thread_id) {
            if entry.status == HistoryStatus::Ready {
                entry.status = HistoryStatus::Stale;
                entry.reason = Some(reason.into());
            }
        }
    }

    /// Records that a thread's history could not be loaded.
    ///
    /// A reason is kept even when a stale baseline exists, because "here is an
    /// old view and here is why it may be old" is a different answer from
    /// "here is the conversation".
    pub fn mark_unavailable(
        &self,
        thread_id: &ThreadId,
        binding: CacheBinding,
        reason: impl Into<String>,
    ) {
        let reason = reason.into();
        let mut inner = self.lock();
        inner.next_tick += 1;
        let tick = inner.next_tick;

        // The match is resolved before the mutable borrow starts: the entry is
        // either updated in place (it describes this same conversation) or
        // replaced, which needs `inner` for more than the map.
        let same_conversation = inner
            .entries
            .get(thread_id)
            .is_some_and(|entry| entry.binding.as_ref() == Some(&binding));

        if same_conversation {
            let entry = inner
                .entries
                .get_mut(thread_id)
                .expect("the entry was checked above");
            entry.status = HistoryStatus::Unavailable;
            entry.reason = Some(reason);
            entry.last_used = tick;
            return;
        }

        let generation = inner.take_generation();
        inner.remove(thread_id);
        inner.insert(
            thread_id.clone(),
            Entry {
                generation,
                binding: Some(binding),
                status: HistoryStatus::Unavailable,
                reason: Some(reason),
                rows: Vec::new(),
                next_seq: 1,
                bytes: 0,
                last_used: tick,
            },
        );
        inner.evict_to_fit(self.max_threads, self.max_total_bytes);
    }

    /// Records that a load is in flight for a thread.
    pub fn mark_loading(&self, thread_id: &ThreadId, binding: CacheBinding) {
        let mut inner = self.lock();
        inner.next_tick += 1;
        let tick = inner.next_tick;

        // A cached conversation stays visible while it is refreshed: it is
        // stale, not loading, because there is something to show. Resolved
        // before the mutable borrow for the same reason as `mark_unavailable`.
        let refresh = inner
            .entries
            .get(thread_id)
            .is_some_and(|entry| {
                entry.binding.as_ref() == Some(&binding) && !entry.rows.is_empty()
            });

        if refresh {
            let entry = inner
                .entries
                .get_mut(thread_id)
                .expect("the entry was checked above");
            entry.status = HistoryStatus::Stale;
            entry.reason = Some("a newer view is being loaded".to_owned());
            entry.last_used = tick;
            return;
        }

        let generation = inner.take_generation();
        inner.remove(thread_id);
        inner.insert(
            thread_id.clone(),
            Entry {
                generation,
                binding: Some(binding),
                status: HistoryStatus::Loading,
                reason: None,
                rows: Vec::new(),
                next_seq: 1,
                bytes: 0,
                last_used: tick,
            },
        );
    }

    /// The cached conversation for a thread, if there is one.
    pub fn view(&self, thread_id: &ThreadId) -> Option<CacheView> {
        let mut inner = self.lock();
        inner.next_tick += 1;
        let tick = inner.next_tick;
        let entry = inner.entries.get_mut(thread_id)?;
        entry.last_used = tick;
        Some(CacheView {
            generation: entry.generation,
            status: entry.status,
            complete: entry.status == HistoryStatus::Ready,
            reason: entry.reason.clone(),
            rows: entry.rows.clone(),
        })
    }

    /// The generation a thread is on, without copying its rows.
    pub fn generation(&self, thread_id: &ThreadId) -> Option<u64> {
        let mut inner = self.lock();
        inner.next_tick += 1;
        let tick = inner.next_tick;
        let entry = inner.entries.get_mut(thread_id)?;
        entry.last_used = tick;
        Some(entry.generation)
    }

    /// Drops a thread's cached conversation, if any.
    pub fn remove(&self, thread_id: &ThreadId) {
        let mut inner = self.lock();
        inner.remove(thread_id);
    }

    /// How many conversations are cached.
    pub fn len(&self) -> usize {
        self.lock().entries.len()
    }

    /// Whether the cache holds nothing.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Bytes of serialized rows currently held.
    pub fn bytes(&self) -> u64 {
        self.lock().bytes
    }

    /// Whether a load is in flight for a thread.
    pub fn is_loading(&self, thread_id: &ThreadId) -> bool {
        self.lock().loads.contains_key(thread_id)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Inner {
    /// Mints a generation. Never reused, so a cursor from an older one can
    /// never be read as a position in a newer one.
    fn take_generation(&mut self) -> u64 {
        let generation = self.next_generation;
        self.next_generation += 1;
        generation
    }

    fn insert(&mut self, thread_id: ThreadId, mut entry: Entry) {
        self.next_tick += 1;
        entry.last_used = self.next_tick;
        self.bytes = self.bytes.saturating_add(entry.bytes);
        self.entries.insert(thread_id, entry);
    }

    fn remove(&mut self, thread_id: &ThreadId) -> Option<Entry> {
        let entry = self.entries.remove(thread_id)?;
        self.bytes = self.bytes.saturating_sub(entry.bytes);
        Some(entry)
    }

    /// Drops least-recently-used conversations until both bounds hold.
    ///
    /// Whole conversations are dropped, never rows from the middle: a partial
    /// conversation that still claims to be the conversation is the one
    /// outcome a caller cannot detect.
    fn evict_to_fit(&mut self, max_threads: usize, max_total_bytes: u64) {
        while self.entries.len() > max_threads || self.bytes > max_total_bytes {
            let Some(victim) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(thread_id, _)| thread_id.clone())
            else {
                return;
            };
            self.remove(&victim);
        }
    }
}

/// The serialized size of a row, which is what the budget is measured in.
fn row_bytes(event: &ProviderEvent) -> u64 {
    serde_json::to_vec(event)
        .map(|encoded| encoded.len() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binding(agent: &str) -> CacheBinding {
        CacheBinding {
            host_id: HostId::mint(),
            agent: agent.to_owned(),
            provider_session_id: "acp-session-1".to_owned(),
            cwd: "/srv/project".to_owned(),
        }
    }

    fn identity() -> ProviderEvent {
        ProviderEvent::ThreadIdentity {
            provider_thread_id: "acp-session-1".into(),
        }
    }

    fn cache() -> HistoryCache {
        HistoryCache::new(8, 1024 * 1024, 4)
    }

    #[test]
    fn a_baseline_is_numbered_from_one_under_a_new_generation() {
        let cache = cache();
        let thread = ThreadId::mint();
        let generation = cache.install_baseline(&thread, binding("pi"), vec![identity(), identity()]);

        let view = cache.view(&thread).expect("the baseline is cached");
        assert_eq!(view.generation, generation);
        assert_eq!(view.status, HistoryStatus::Ready);
        assert!(view.complete);
        assert_eq!(
            view.rows.iter().map(|row| row.seq).collect::<Vec<_>>(),
            vec![1, 2]
        );
    }

    /// The rule the relay's index-derived `sourceSeq` broke: a row's number
    /// comes from a counter that only moves forward, so nothing that happens to
    /// the rows around it can change it.
    #[test]
    fn live_events_continue_the_sequence() {
        let cache = cache();
        let thread = ThreadId::mint();
        let binding = binding("pi");
        cache.install_baseline(&thread, binding.clone(), vec![identity()]);

        assert_eq!(
            cache.append_live(&thread, Some(&binding), identity()),
            Some(2)
        );
        assert_eq!(
            cache.append_live(&thread, Some(&binding), identity()),
            Some(3)
        );

        let view = cache.view(&thread).unwrap();
        assert_eq!(
            view.rows.iter().map(|row| row.seq).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
    }

    #[test]
    fn a_rebuild_mints_a_generation_that_is_never_reused() {
        let cache = cache();
        let thread = ThreadId::mint();
        let binding = binding("pi");
        let first = cache.install_baseline(&thread, binding.clone(), vec![identity()]);
        let second = cache.install_baseline(&thread, binding, vec![identity(), identity()]);
        assert!(second > first, "a rebuild must not reuse a generation");

        // A new baseline renumbers from one, which is exactly why the client
        // needs the generation to tell the two numberings apart.
        let view = cache.view(&thread).unwrap();
        assert_eq!(view.generation, second);
        assert_eq!(
            view.rows.iter().map(|row| row.seq).collect::<Vec<_>>(),
            vec![1, 2]
        );
    }

    /// An event from a conversation the cache is not holding must not land in
    /// the one it is.
    #[test]
    fn a_live_event_for_another_binding_is_refused() {
        let cache = cache();
        let thread = ThreadId::mint();
        cache.install_baseline(&thread, binding("pi"), vec![identity()]);
        assert_eq!(
            cache.append_live(&thread, Some(&binding("omp")), identity()),
            None
        );
        assert_eq!(cache.view(&thread).unwrap().rows.len(), 1);
    }

    /// A thread whose agent has not reported a session yet still has a
    /// conversation as far as the user is concerned: the message they just
    /// sent. It is shown, and it is marked partial — nothing has confirmed it
    /// is the whole conversation, and the next load replaces it with the
    /// baseline the agent replays.
    #[test]
    fn a_live_event_without_a_baseline_starts_a_partial_conversation() {
        let cache = cache();
        let thread = ThreadId::mint();
        assert_eq!(cache.append_live(&thread, None, identity()), Some(1));

        let view = cache.view(&thread).expect("the conversation is shown");
        assert_eq!(view.status, HistoryStatus::Partial);
        assert!(!view.complete, "an overlay is not a whole conversation");
        assert_eq!(view.rows.len(), 1);
    }

    /// The first event that names a session adopts it, so the load that follows
    /// knows what to ask for.
    #[test]
    fn a_partial_conversation_adopts_the_first_binding_it_sees() {
        let cache = cache();
        let thread = ThreadId::mint();
        let pi = binding("pi");
        cache.append_live(&thread, None, identity());
        assert_eq!(
            cache.append_live(&thread, Some(&pi), identity()),
            Some(2)
        );
        // A different conversation is still refused.
        assert_eq!(
            cache.append_live(&thread, Some(&binding("omp")), identity()),
            None
        );
    }

    /// A real empty session is `Ready` with no rows, which is a different
    /// answer from "could not load".
    #[test]
    fn an_empty_session_is_ready_and_not_unavailable() {
        let cache = cache();
        let thread = ThreadId::mint();
        cache.install_baseline(&thread, binding("pi"), Vec::new());
        let view = cache.view(&thread).unwrap();
        assert_eq!(view.status, HistoryStatus::Ready);
        assert!(view.complete);
        assert!(view.rows.is_empty());
        assert_eq!(view.reason, None);
    }

    #[test]
    fn an_unavailable_thread_carries_its_reason() {
        let cache = cache();
        let thread = ThreadId::mint();
        cache.mark_unavailable(&thread, binding("pi"), "the session no longer exists");
        let view = cache.view(&thread).unwrap();
        assert_eq!(view.status, HistoryStatus::Unavailable);
        assert!(!view.complete);
        assert_eq!(
            view.reason.as_deref(),
            Some("the session no longer exists")
        );
    }

    /// A refresh must not blank the conversation the user is looking at.
    #[test]
    fn refreshing_keeps_the_cached_rows_visible_as_stale() {
        let cache = cache();
        let thread = ThreadId::mint();
        let binding = binding("pi");
        cache.install_baseline(&thread, binding.clone(), vec![identity()]);
        cache.mark_loading(&thread, binding);
        let view = cache.view(&thread).unwrap();
        assert_eq!(view.status, HistoryStatus::Stale);
        assert!(!view.complete);
        assert_eq!(view.rows.len(), 1, "the cached view stays visible");
    }

    #[test]
    fn a_first_load_with_nothing_cached_reports_loading() {
        let cache = cache();
        let thread = ThreadId::mint();
        cache.mark_loading(&thread, binding("pi"));
        let view = cache.view(&thread).unwrap();
        assert_eq!(view.status, HistoryStatus::Loading);
        assert!(view.rows.is_empty());
    }

    #[test]
    fn one_loader_per_thread_and_binding() {
        let cache = cache();
        let thread = ThreadId::mint();
        let binding = binding("pi");
        assert_eq!(cache.begin_load(&thread, &binding), LoadTicket::Leader);
        assert_eq!(cache.begin_load(&thread, &binding), LoadTicket::Follower);
        assert!(cache.is_loading(&thread));

        cache.finish_load(&thread);
        assert!(!cache.is_loading(&thread));
        assert_eq!(cache.begin_load(&thread, &binding), LoadTicket::Leader);
    }

    /// A binding change makes it a different load, not the one already running.
    #[test]
    fn a_changed_binding_is_admitted_as_its_own_load() {
        let cache = cache();
        let thread = ThreadId::mint();
        assert_eq!(cache.begin_load(&thread, &binding("pi")), LoadTicket::Leader);
        assert_eq!(cache.begin_load(&thread, &binding("omp")), LoadTicket::Leader);
    }

    #[test]
    fn concurrent_loads_are_bounded() {
        let cache = HistoryCache::new(8, 1024 * 1024, 1);
        assert_eq!(
            cache.begin_load(&ThreadId::mint(), &binding("pi")),
            LoadTicket::Leader
        );
        assert_eq!(
            cache.begin_load(&ThreadId::mint(), &binding("pi")),
            LoadTicket::Refused
        );
    }

    #[test]
    fn the_least_recently_used_thread_is_evicted_first() {
        let cache = HistoryCache::new(2, 1024 * 1024, 4);
        let first = ThreadId::mint();
        let second = ThreadId::mint();
        let third = ThreadId::mint();
        cache.install_baseline(&first, binding("pi"), vec![identity()]);
        cache.install_baseline(&second, binding("pi"), vec![identity()]);
        // Reading `first` makes `second` the least recently used.
        cache.view(&first).unwrap();

        cache.install_baseline(&third, binding("pi"), vec![identity()]);
        assert!(cache.view(&first).is_some(), "the recent thread survives");
        assert!(cache.view(&second).is_none(), "the stale thread is evicted");
        assert!(cache.view(&third).is_some());
    }

    /// Eviction drops whole conversations: a partial one that still claimed to
    /// be the conversation is the failure a caller cannot detect.
    #[test]
    fn the_byte_budget_evicts_whole_conversations() {
        let one_row = row_bytes(&identity());
        let cache = HistoryCache::new(8, one_row * 2 + one_row / 2, 4);
        let first = ThreadId::mint();
        let second = ThreadId::mint();
        cache.install_baseline(&first, binding("pi"), vec![identity(), identity()]);
        cache.install_baseline(&second, binding("pi"), vec![identity(), identity()]);
        assert!(cache.bytes() <= one_row * 2 + one_row / 2);
        assert_eq!(cache.len(), 1, "the older conversation is dropped whole");
    }

    #[test]
    fn removing_a_thread_frees_its_bytes() {
        let cache = cache();
        let thread = ThreadId::mint();
        cache.install_baseline(&thread, binding("pi"), vec![identity()]);
        assert!(cache.bytes() > 0);
        cache.remove(&thread);
        assert_eq!(cache.bytes(), 0);
        assert!(cache.view(&thread).is_none());
    }
}
