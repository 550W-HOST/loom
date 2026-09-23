//! The in-memory overlay: a conversation's rows before they are written.
//!
//! The store is what holds a conversation. This holds the small part of it that
//! exists only in memory so far — the rows a publish accepted and the writer has
//! not committed yet — plus the bookkeeping for **loads in flight**, which is
//! in-memory by nature: who is loading which thread from which binding, so ten
//! readers of one conversation are one load and ten readers.
//!
//! What it deliberately no longer holds is the conversation. A baseline and a
//! generation used to live here, and a restart threw both away: a cursor from
//! before the restart was a position in a numbering that no longer existed, and
//! the rows behind it were gone. The store keeps the baseline durably now, and
//! mints the instance id that says which numbering a cursor belongs to, so all
//! that is left here is the tail that has not reached the disk.
//!
//! Two rules, both about identity:
//!
//! * **A sequence arrives with the row.** The publisher reserves it (see
//!   [`crate::store::SeqAllocator`]) and the store writes the row under the same
//!   number, so a row does not change name when it moves from here to there, and
//!   trimming written rows never renumbers the survivors.
//! * **A binding is a conversation.** A row offered for a thread that is already
//!   bound to a different session is refused: showing one conversation's rows
//!   under another's name is the failure a reader cannot detect.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use loom_domain::{HostId, ProviderEvent, RunId, ThreadId};

/// What a conversation is bound to.
///
/// Every part matters: the session id names a file on one host's disk, issued
/// by one agent, meaningful for one directory. An entry whose binding no longer
/// matches the thread is not a stale view of the same conversation — it is a
/// different conversation, and using it would show one thread's history under
/// another's name.
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

/// How much of a conversation a reader can currently be offered.
///
/// The values are derived from what the store holds and whether a load is in
/// flight, which is what makes them survive a restart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HistoryStatus {
    /// A load is in flight and there is nothing to show yet.
    Loading,
    /// Rows exist but no baseline has ever been loaded: what the user said, and
    /// nothing that confirms it is the whole conversation.
    Partial,
    /// A complete baseline is stored.
    Ready,
    /// A complete baseline is stored and a newer view is pending, or the last
    /// attempt to get one failed.
    Stale,
    /// There is nothing to show, and `reason` says why.
    Unavailable,
}

impl HistoryStatus {
    /// The token the timeline contract carries this status as.
    pub fn token(self) -> &'static str {
        match self {
            Self::Loading => "loading",
            Self::Partial => "partial",
            Self::Ready => "ready",
            Self::Stale => "stale",
            Self::Unavailable => "unavailable",
        }
    }
}

/// Where one row came from.
///
/// The distinction is not bookkeeping: a row's origin decides what its
/// projection may say about it. A frame loom published while the thread was
/// live knows which turn it belongs to and when it happened; a frame the agent
/// replayed when the conversation was loaded knows neither, and inventing
/// either would misdate the conversation or offer an action on a run that never
/// existed. Both end up in the same conversation, in the order they happened,
/// which is why the origin has to travel *with* the row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RowSource {
    /// A message loom recorded itself — the user's prompt, or a reply posted
    /// through the messages API — rather than a frame a run published. It knows
    /// when it was said, and belongs to no run.
    Message { at_ms: u64 },
    /// A frame a run published. The run is its grouping key, and `at_ms` is when
    /// the run published it.
    Run { run_id: RunId, at_ms: u64 },
    /// A frame the agent replayed when the conversation was loaded. It carries
    /// no loom time and belongs to no run: a replay is a reconstruction, not a
    /// record of when things happened.
    Replayed,
}

/// One row of a conversation: a stable sequence, where it came from, and the
/// frame behind it.
#[derive(Clone, Debug, PartialEq)]
pub struct CachedRow {
    /// The number its publisher reserved. Assigned once, never recomputed.
    pub seq: u64,
    /// Where the frame came from, which its projection must respect.
    pub source: RowSource,
    /// The contract event a projection turns into a timeline row.
    pub event: ProviderEvent,
}

impl From<crate::store::StoredRow> for CachedRow {
    fn from(row: crate::store::StoredRow) -> Self {
        Self {
            seq: row.seq,
            source: row.source,
            event: row.event,
        }
    }
}

/// A conversation to serve: the rows, and the identity they are a position in.
#[derive(Clone, Debug, PartialEq)]
pub struct CacheView {
    /// Which numbering produced this view.
    ///
    /// The store's identity: it is minted with the file and kept, so it
    /// survives a restart. A client holding `(instance, generation)` can tell
    /// "this conversation was rebuilt" from "the server restarted", which a bare
    /// integer cannot — comparing integers across a restart reads a new
    /// numbering as a rollback and would discard every page it ever sends.
    pub instance: String,
    /// The revision every `seq` in `rows` belongs to, inside [`CacheView::instance`].
    /// Durable, and only moved by a rebuild.
    pub generation: u64,
    /// How much can be offered.
    pub status: HistoryStatus,
    /// Whether the rows are a complete conversation. A `Ready` view with no rows
    /// is a real empty session; an incomplete one is not empty, it is partial,
    /// and saying so is the difference the caller must not lose.
    pub complete: bool,
    /// Whether the server's locally observed rows are a complete history source.
    pub local_complete: bool,
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

/// A thread's unwritten rows.
struct Entry {
    /// What this conversation is bound to, when a session is known.
    ///
    /// `None` is a thread the agent has not reported a session for yet: it can
    /// still show what the user has said, but nothing has confirmed it is the
    /// whole conversation.
    binding: Option<CacheBinding>,
    /// Rows published but not yet committed, oldest first.
    rows: Vec<CachedRow>,
    /// Bytes of the serialized rows, for the budget.
    bytes: u64,
    /// Monotone access counter, so least-recently-used is exact and testable.
    last_used: u64,
}

struct Inner {
    entries: HashMap<ThreadId, Entry>,
    bytes: u64,
    next_tick: u64,
    /// Loads in flight, keyed by thread. A second caller for the same thread
    /// waits rather than loading the same conversation twice.
    loads: HashMap<ThreadId, CacheBinding>,
    /// When a thread's last attempt to load failed, if it did.
    ///
    /// This is what keeps a failure from being retried by every single read: a
    /// reader that finds the conversation old will ask again, and asking an
    /// agent that is down is slow enough that asking on every poll is a way to
    /// make a bad situation worse. In memory on purpose — how long to wait is a
    /// property of this process, not of the conversation.
    failed_at: HashMap<ThreadId, Instant>,
}

/// The unwritten tail of every conversation, and the loads in flight.
pub struct HistoryCache {
    inner: Mutex<Inner>,
    max_threads: usize,
    max_total_bytes: u64,
    max_concurrent_loads: usize,
}

impl HistoryCache {
    /// Creates an overlay holding at most `max_threads` threads' unwritten rows
    /// and `max_total_bytes` of them.
    pub fn new(max_threads: usize, max_total_bytes: u64, max_concurrent_loads: usize) -> Self {
        Self {
            inner: Mutex::new(Inner {
                entries: HashMap::new(),
                bytes: 0,
                next_tick: 0,
                loads: HashMap::new(),
                failed_at: HashMap::new(),
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
        // Touch so a thread that just failed to load is not the first candidate
        // for eviction: it was used most recently.
        inner.touch(thread_id);
    }

    /// Whether a load is in flight for a thread.
    pub fn is_loading(&self, thread_id: &ThreadId) -> bool {
        self.lock().loads.contains_key(thread_id)
    }

    /// Records that a thread's last attempt to load failed.
    pub fn mark_sync_failed(&self, thread_id: &ThreadId) {
        self.lock()
            .failed_at
            .insert(thread_id.clone(), Instant::now());
    }

    /// Records that a thread's conversation is current again.
    pub fn clear_sync_failure(&self, thread_id: &ThreadId) {
        self.lock().failed_at.remove(thread_id);
    }

    /// Whether enough time has passed to try a failed load again.
    ///
    /// A thread that has never failed may always be tried. One that just failed
    /// waits `backoff` first, so a reader polling an agent that is down does not
    /// turn its own page into a stream of failed loads.
    pub fn may_retry(&self, thread_id: &ThreadId, backoff: Duration) -> bool {
        let mut inner = self.lock();
        match inner.failed_at.get(thread_id) {
            None => true,
            Some(failed_at) if failed_at.elapsed() >= backoff => {
                // The wait is over: this attempt is the retry, and it records
                // its own outcome like any other.
                inner.failed_at.remove(thread_id);
                true
            }
            Some(_) => false,
        }
    }

    /// Records which conversation a thread's rows now belong to.
    ///
    /// An install that lands in the store is what says which session a thread's
    /// conversation now is; rows that arrive after it are checked against that,
    /// not against whatever was known before.
    pub fn adopt_binding(&self, thread_id: &ThreadId, binding: &CacheBinding) {
        let mut inner = self.lock();
        inner.next_tick += 1;
        let tick = inner.next_tick;
        if let Some(entry) = inner.entries.get_mut(thread_id) {
            entry.binding = Some(binding.clone());
            entry.last_used = tick;
            return;
        }
        inner.insert(
            thread_id.clone(),
            Entry {
                binding: Some(binding.clone()),
                rows: Vec::new(),
                bytes: 0,
                last_used: tick,
            },
        );
    }

    /// Appends one live event to a thread's unwritten rows.
    ///
    /// Returns the sequence it was given, or `None` when the event was refused.
    /// A refusal is the safe answer for a mismatched binding: this event belongs
    /// to a conversation this thread is not holding.
    pub fn append_live(
        &self,
        thread_id: &ThreadId,
        binding: Option<&CacheBinding>,
        seq: u64,
        source: RowSource,
        event: ProviderEvent,
    ) -> Option<u64> {
        let mut inner = self.lock();
        inner.next_tick += 1;
        let tick = inner.next_tick;
        let size = row_bytes(&event);
        let binding = binding.cloned();

        if !inner.entries.contains_key(thread_id) {
            // A thread with no entry yet is one whose conversation has never
            // been bound here: a brand-new conversation, or one whose first run
            // is still in flight. What the user has already said belongs on
            // screen, so the row is kept and a load that follows checks the
            // binding against it.
            inner.insert(
                thread_id.clone(),
                Entry {
                    binding: None,
                    rows: Vec::new(),
                    bytes: 0,
                    last_used: tick,
                },
            );
        }

        {
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
            entry.bytes = entry.bytes.saturating_add(size);
            entry.rows.push(CachedRow { seq, source, event });
            entry.last_used = tick;
        }

        inner.bytes = inner.bytes.saturating_add(size);
        if inner.bytes > self.max_total_bytes || inner.entries.len() > self.max_threads {
            inner.evict_to_fit(self.max_threads, self.max_total_bytes);
        }
        Some(seq)
    }

    /// Drops the rows a thread now has stored, up to and including `seq`.
    ///
    /// The store is the conversation; this only exists so a committed row is not
    /// held twice. A row the store refused stays here, which is what keeps it on
    /// screen and keeps the thread's stored state marked incomplete.
    pub fn confirm_written(&self, thread_id: &ThreadId, seq: u64) {
        let mut inner = self.lock();
        let Some(entry) = inner.entries.get_mut(thread_id) else {
            return;
        };
        let before = entry.bytes;
        entry.rows.retain(|row| row.seq > seq);
        entry.bytes = entry.rows.iter().map(|row| row_bytes(&row.event)).sum();
        let freed = before.saturating_sub(entry.bytes);
        inner.bytes = inner.bytes.saturating_sub(freed);
    }

    /// A thread's unwritten rows, oldest first.
    pub fn rows(&self, thread_id: &ThreadId) -> Option<Vec<CachedRow>> {
        let mut inner = self.lock();
        inner.touch(thread_id);
        inner.entries.get(thread_id).map(|entry| entry.rows.clone())
    }

    /// The first Loom-authored user prompt title in the unwritten tail.
    ///
    /// A prompt can reach the thread list before its asynchronous store write,
    /// so the display fallback must inspect this overlay as well as the store.
    pub(crate) fn first_user_prompt_title(&self, thread_id: &ThreadId) -> Option<String> {
        let mut inner = self.lock();
        inner.touch(thread_id);
        inner.entries.get(thread_id)?.rows.iter().find_map(|row| {
            matches!(&row.source, RowSource::Message { .. })
                .then(|| first_user_prompt_title(&row.event))
                .flatten()
        })
    }

    /// What a thread's unwritten rows are bound to, if anything is known.
    pub fn binding(&self, thread_id: &ThreadId) -> Option<CacheBinding> {
        self.lock()
            .entries
            .get(thread_id)
            .and_then(|entry| entry.binding.clone())
    }

    /// Drops a thread's unwritten rows.
    pub fn remove(&self, thread_id: &ThreadId) {
        let mut inner = self.lock();
        inner.remove(thread_id);
    }

    /// How many threads hold unwritten rows.
    pub fn len(&self) -> usize {
        self.lock().entries.len()
    }

    /// Whether anything is held.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Bytes of serialized rows currently held.
    pub fn bytes(&self) -> u64 {
        self.lock().bytes
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Inner {
    /// Marks a thread as used just now, so eviction's order stays exact.
    fn touch(&mut self, thread_id: &ThreadId) {
        self.next_tick += 1;
        let tick = self.next_tick;
        if let Some(entry) = self.entries.get_mut(thread_id) {
            entry.last_used = tick;
        }
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

    /// Drops least-recently-used threads until both bounds hold.
    ///
    /// What is dropped is the *unwritten* tail: the stored conversation is not
    /// here to lose. A dropped row is a row the writer refused or never reached,
    /// which the thread's stored state already reports as incomplete.
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

const TITLE_FALLBACK_MAX_CHARS: usize = 120;

/// Converts the first line of a Loom-authored user message into a compact
/// sidebar label. Replayed provider history is excluded by callers.
pub(crate) fn first_user_prompt_title(event: &ProviderEvent) -> Option<String> {
    let ProviderEvent::ItemStarted {
        item: loom_domain::ThreadEventItem::UserMessage { content, .. },
        ..
    } = event
    else {
        return None;
    };

    let mut title = String::new();
    let mut char_count = 0;
    let mut truncated = false;
    'content: for part in content {
        let loom_domain::UserContent::Text { text } = part else {
            continue;
        };
        for character in text.chars() {
            if matches!(character, '\r' | '\n') {
                break 'content;
            }
            if character.is_control() || (title.is_empty() && character.is_whitespace()) {
                continue;
            }
            if char_count == TITLE_FALLBACK_MAX_CHARS {
                truncated = true;
                break 'content;
            }
            title.push(character);
            char_count += 1;
        }
    }

    let title = title.trim_end();
    if title.is_empty() {
        return None;
    }
    if truncated {
        Some(format!("{title}…"))
    } else {
        Some(title.to_owned())
    }
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

    /// A live frame, as a run publishes it.
    fn live() -> RowSource {
        RowSource::Run {
            run_id: RunId::mint(),
            at_ms: 1_700_000_000_000,
        }
    }

    fn identity() -> ProviderEvent {
        ProviderEvent::ThreadIdentity {
            provider_thread_id: "acp-session-1".into(),
        }
    }
    fn user_message(text: &str) -> ProviderEvent {
        ProviderEvent::ItemStarted {
            item: loom_domain::ThreadEventItem::UserMessage {
                id: "user-1".into(),
                content: vec![loom_domain::UserContent::Text {
                    text: text.to_owned(),
                }],
                client_request_id: None,
                parent_tool_call_id: None,
            },
            provider_thread_id: "acp-session-1".into(),
        }
    }

    fn cache() -> HistoryCache {
        HistoryCache::new(8, 1024 * 1024, 4)
    }

    #[test]
    fn the_first_unwritten_user_prompt_provides_the_title_fallback() {
        let cache = cache();
        let thread = ThreadId::mint();
        cache.append_live(&thread, None, 1, live(), identity());
        cache.append_live(
            &thread,
            None,
            2,
            RowSource::Message { at_ms: 2 },
            user_message("  Name this thread\nmore details"),
        );
        cache.append_live(
            &thread,
            None,
            3,
            RowSource::Message { at_ms: 3 },
            user_message("later prompt"),
        );

        assert_eq!(
            cache.first_user_prompt_title(&thread).as_deref(),
            Some("Name this thread")
        );
    }

    #[test]
    fn the_title_fallback_is_bounded_and_uses_one_line() {
        let text = format!("{}x\nignored", "a".repeat(TITLE_FALLBACK_MAX_CHARS));
        assert_eq!(
            first_user_prompt_title(&user_message(&text)),
            Some(format!("{}…", "a".repeat(TITLE_FALLBACK_MAX_CHARS)))
        );
    }

    #[test]
    fn rows_arrive_with_the_sequence_they_were_reserved() {
        let cache = cache();
        let thread = ThreadId::mint();
        assert_eq!(
            cache.append_live(&thread, None, 7, live(), identity()),
            Some(7)
        );
        assert_eq!(
            cache.append_live(&thread, None, 9, live(), identity()),
            Some(9)
        );
        let rows = cache.rows(&thread).unwrap();
        assert_eq!(
            rows.iter().map(|row| row.seq).collect::<Vec<_>>(),
            vec![7, 9],
            "the overlay keeps the numbering it was given"
        );
    }

    /// Where a row came from is part of the row, not something a projection can
    /// recover: a live frame knows its turn and its time, and a replayed one
    /// knows neither.
    #[test]
    fn a_rows_origin_travels_with_it() {
        let cache = cache();
        let thread = ThreadId::mint();
        let run_id = RunId::mint();
        cache.append_live(
            &thread,
            None,
            1,
            RowSource::Run {
                run_id: run_id.clone(),
                at_ms: 1_700_000_000_000,
            },
            identity(),
        );
        cache.append_live(
            &thread,
            None,
            2,
            RowSource::Message { at_ms: 5 },
            identity(),
        );

        let rows = cache.rows(&thread).unwrap();
        assert_eq!(
            rows[0].source,
            RowSource::Run {
                run_id,
                at_ms: 1_700_000_000_000
            }
        );
        assert_eq!(rows[1].source, RowSource::Message { at_ms: 5 });
    }

    /// An event from a conversation the overlay is not holding must not land in
    /// the one it is.
    #[test]
    fn a_live_event_for_another_binding_is_refused() {
        let cache = cache();
        let thread = ThreadId::mint();
        cache.append_live(&thread, Some(&binding("pi")), 1, live(), identity());
        assert_eq!(
            cache.append_live(&thread, Some(&binding("omp")), 2, live(), identity()),
            None
        );
        assert_eq!(cache.rows(&thread).unwrap().len(), 1);
    }

    /// The first event that names a session adopts it, so a load that follows
    /// knows what to ask for.
    #[test]
    fn a_conversation_adopts_the_first_binding_it_sees() {
        let cache = cache();
        let thread = ThreadId::mint();
        let pi = binding("pi");
        cache.append_live(&thread, None, 1, live(), identity());
        assert_eq!(
            cache.append_live(&thread, Some(&pi), 2, live(), identity()),
            Some(2)
        );
        assert_eq!(cache.binding(&thread), Some(pi));
        // A different conversation is still refused.
        assert_eq!(
            cache.append_live(&thread, Some(&binding("omp")), 3, live(), identity()),
            None
        );
    }

    /// A row that reached the store is dropped from the overlay; the ones after
    /// it stay, because they have not.
    #[test]
    fn confirming_a_write_drops_only_what_it_covers() {
        let cache = cache();
        let thread = ThreadId::mint();
        for seq in 1..=3 {
            cache.append_live(&thread, None, seq, live(), identity());
        }
        cache.confirm_written(&thread, 2);
        let rows = cache.rows(&thread).unwrap();
        assert_eq!(
            rows.iter().map(|row| row.seq).collect::<Vec<_>>(),
            vec![3],
            "the committed prefix is gone and the rest is not"
        );
        assert!(cache.bytes() > 0, "what is left is still counted");

        cache.confirm_written(&thread, 3);
        assert!(cache.rows(&thread).unwrap().is_empty());
        assert_eq!(cache.bytes(), 0, "the bytes go with the rows");
    }

    /// A rejected row is never confirmed, so it stays on screen: the store's
    /// refusal is what the thread's state reports, not a hole in the view.
    #[test]
    fn a_row_the_store_never_wrote_stays() {
        let cache = cache();
        let thread = ThreadId::mint();
        cache.append_live(&thread, None, 1, live(), identity());
        cache.confirm_written(&thread, 0);
        assert_eq!(cache.rows(&thread).unwrap().len(), 1);
    }

    #[test]
    fn an_install_adopts_the_binding_of_the_conversation_it_wrote() {
        let cache = cache();
        let thread = ThreadId::mint();
        let installed = binding("omp");
        cache.append_live(&thread, Some(&binding("pi")), 1, live(), identity());
        cache.adopt_binding(&thread, &installed);
        assert_eq!(cache.binding(&thread), Some(installed));
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
        assert_eq!(
            cache.begin_load(&thread, &binding("pi")),
            LoadTicket::Leader
        );
        assert_eq!(
            cache.begin_load(&thread, &binding("omp")),
            LoadTicket::Leader
        );
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
        cache.append_live(&first, None, 1, live(), identity());
        cache.append_live(&second, None, 1, live(), identity());
        // Reading `first` makes `second` the least recently used.
        cache.rows(&first).unwrap();

        cache.append_live(&third, None, 1, live(), identity());
        assert!(cache.rows(&first).is_some(), "the recent thread survives");
        assert!(cache.rows(&second).is_none(), "the stale thread is evicted");
        assert!(cache.rows(&third).is_some());
    }

    #[test]
    fn the_byte_budget_evicts_whole_threads() {
        let one_row = row_bytes(&identity());
        let cache = HistoryCache::new(8, one_row * 2 + one_row / 2, 4);
        let first = ThreadId::mint();
        let second = ThreadId::mint();
        for seq in 1..=2 {
            cache.append_live(&first, None, seq, live(), identity());
            cache.append_live(&second, None, seq, live(), identity());
        }
        assert!(cache.bytes() <= one_row * 2 + one_row / 2);
        assert_eq!(cache.len(), 1, "the older thread's rows are dropped whole");
    }

    /// A failure is not retried by every read: asking an agent that is down is
    /// slow, and a polling reader must not turn one failure into a stream.
    #[test]
    fn a_failed_load_waits_before_it_is_tried_again() {
        let cache = cache();
        let thread = ThreadId::mint();
        assert!(cache.may_retry(&thread, Duration::from_secs(30)));

        cache.mark_sync_failed(&thread);
        assert!(
            !cache.may_retry(&thread, Duration::from_secs(30)),
            "a fresh failure is not retried immediately"
        );
        assert!(
            cache.may_retry(&thread, Duration::ZERO),
            "and waiting long enough opens it again"
        );
        // Admitting a retry consumes the failure: the attempt that was just let
        // through records its own outcome, so an attempt still in flight is not
        // blocked here but by the load claim.
        assert!(
            cache.may_retry(&thread, Duration::from_secs(30)),
            "the admitted retry is no longer holding the gate shut"
        );

        cache.mark_sync_failed(&thread);
        cache.clear_sync_failure(&thread);
        assert!(
            cache.may_retry(&thread, Duration::from_secs(30)),
            "a conversation that is current again needs no wait"
        );
    }

    #[test]
    fn removing_a_thread_frees_its_bytes() {
        let cache = cache();
        let thread = ThreadId::mint();
        cache.append_live(&thread, None, 1, live(), identity());
        assert!(cache.bytes() > 0);
        cache.remove(&thread);
        assert_eq!(cache.bytes(), 0);
        assert!(cache.rows(&thread).is_none());
    }
}
