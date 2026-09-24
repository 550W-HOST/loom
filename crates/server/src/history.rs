//! Loading a thread's conversation on demand.
//!
//! The cache holds what has been loaded; this decides when a load happens and
//! makes sure only one happens per thread at a time. Ten clients opening the
//! same conversation is one load and ten readers, not ten loads of the same
//! conversation from the same machine.
//!
//! Nothing here decides *what* is in a conversation: the host that owns the
//! session replays it and this installs the result. The one rule it enforces on
//! the way through is that a load either installs a whole baseline or leaves
//! the cache marked unavailable with a reason — never a partial conversation
//! presented as the conversation.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use loom_domain::{HostId, ProviderEvent, ThreadId};
use loom_provider_protocol::HostRpcOperation;
use tokio::sync::Notify;

use crate::history_cache::{CacheBinding, CacheView, CachedRow, HistoryStatus, LoadTicket};
use crate::history_rpc::HistoryTransportError;
use crate::state::AppState;

/// One batch of a load, as framed on the wire.
const HISTORY_MAX_BATCH_BYTES: u64 = 256 * 1024;
/// The whole conversation, above which the load fails rather than truncates.
const HISTORY_MAX_TOTAL_BYTES: u64 = 8 * 1024 * 1024;
/// How long a load may take. It covers an agent's cold start and a full replay.
const HISTORY_LOAD_DEADLINE: Duration = Duration::from_secs(90);
/// How long a thread waits after a failed load before a read may try again.
///
/// Long enough that a reader polling a page does not turn an agent that is down
/// into a stream of failed loads, short enough that an agent coming back is
/// picked up without anyone having to do anything.
const HISTORY_RETRY_BACKOFF: Duration = Duration::from_secs(30);

/// Why a thread's conversation could not be produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HistoryUnavailable {
    /// The thread has no session binding, or its binding names no host, so
    /// there is no conversation this server can ask for.
    NoBinding,
    /// The bound host does not offer the agent the session belongs to.
    UnknownProvider { host_id: HostId, agent: String },
    /// Loads are already at their concurrency bound.
    ///
    /// Refused rather than queued: a queue would hide the bound, and the
    /// caller can retry once another load has finished.
    Busy,
    /// A run owns the thread, so there is nothing to load *yet*.
    ///
    /// The conversation is being written right now: a replay taken now would be
    /// a snapshot without it. The caller shows what it has and asks again; the
    /// load starts by itself once the run ends.
    RunInFlight,
    /// The host reported a failure.
    Host { code: String, message: String },
    /// The load did not finish in time.
    Timeout,
    /// The load finished without producing a conversation.
    Incomplete(String),
}

impl std::fmt::Display for HistoryUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoBinding => f.write_str("the thread has no provider session to load"),
            Self::UnknownProvider { host_id, agent } => write!(
                f,
                "host {host_id} does not offer the agent {agent:?} this session belongs to"
            ),
            Self::Busy => f.write_str("too many history loads are already in flight"),
            Self::RunInFlight => {
                f.write_str("the conversation's history waits for the run in flight")
            }
            Self::Host { code, message } => {
                write!(
                    f,
                    "the host could not load the conversation ({code}): {message}"
                )
            }
            Self::Timeout => f.write_str("loading the conversation took too long"),
            Self::Incomplete(message) => write!(f, "the conversation was incomplete: {message}"),
        }
    }
}

impl From<HistoryTransportError> for HistoryUnavailable {
    fn from(error: HistoryTransportError) -> Self {
        match error {
            HistoryTransportError::Failed { code, message } => Self::Host { code, message },
            HistoryTransportError::Timeout => Self::Timeout,
            other => Self::Incomplete(other.to_string()),
        }
    }
}

/// The signal a thread's waiters park on.
///
/// Kept beside the cache rather than inside it: the cache is about what is
/// cached, this is about who is waiting for it. An entry exists only while a
/// load is in flight, so the map is bounded by concurrent loads rather than by
/// the number of threads ever opened.
#[derive(Default)]
pub struct HistoryWaits {
    signals: Mutex<HashMap<ThreadId, Arc<Notify>>>,
}

impl HistoryWaits {
    /// Creates an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// The signal for a thread, created on first use.
    fn signal(&self, thread_id: &ThreadId) -> Arc<Notify> {
        let mut signals = self.lock();
        Arc::clone(
            signals
                .entry(thread_id.clone())
                .or_insert_with(|| Arc::new(Notify::new())),
        )
    }

    /// Wakes everyone waiting on a thread and drops the signal.
    ///
    /// Dropping is safe because a waiter re-checks whether a load is still in
    /// flight after registering: a waiter that arrives after this has no one
    /// left to wait for and returns the finished result instead of parking.
    fn wake(&self, thread_id: &ThreadId) {
        let signal = self.lock().remove(thread_id);
        if let Some(signal) = signal {
            signal.notify_waiters();
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<ThreadId, Arc<Notify>>> {
        self.signals
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// What a conversation's cheap facts say about it.
///
/// A [`CacheView`] without its rows: everything a reader can know about a
/// conversation without paying for the rows themselves. It exists because the
/// row set is the expensive part of a read — a long conversation is tens of
/// thousands of stored frames — and a reader that already has the rows
/// projected only needs the metadata to decide whether they are still current.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HistoryMeta {
    /// The revision the rows' sequences belong to.
    pub generation: u64,
    /// How much of the conversation the store can currently offer.
    pub status: HistoryStatus,
    /// Whether that is the whole conversation.
    pub complete: bool,
    /// Whether the conversation is complete without a provider replay.
    pub local_complete: bool,
    /// Why it is not complete, when that is known.
    pub reason: Option<String>,
    /// The highest sequence the conversation holds, or zero when it is empty.
    pub last_seq: u64,
}

/// The status a conversation is in, from what the store holds and the two
/// facts that are not rows: whether a load is in flight, and whether a write
/// was refused.
///
/// One function because two readers ask this question — one holding the rows,
/// one holding only the aggregates — and a status that depended on which one
/// asked would be a lie in one of the two answers.
fn derive_history_status(
    history: Option<&crate::store::StoredHistory>,
    empty: bool,
    loading: bool,
    unsaved: Option<String>,
) -> (HistoryStatus, bool, Option<String>) {
    let synced_at_ms = history.and_then(|stored| stored.synced_at_ms);
    let local_complete = history.is_some_and(|stored| stored.local_complete);
    let has_complete_baseline = local_complete || synced_at_ms.is_some();
    let last_error = history.and_then(|stored| stored.last_error.clone());
    let not_loaded = "the conversation's history has not been loaded yet".to_owned();

    let (status, complete, reason) = if loading && has_complete_baseline {
        (
            HistoryStatus::Stale,
            false,
            Some("a newer view is being loaded".to_owned()),
        )
    } else if loading && !empty {
        (HistoryStatus::Partial, false, Some(not_loaded))
    } else if loading {
        (HistoryStatus::Loading, false, None)
    } else if has_complete_baseline {
        match last_error {
            Some(error) => (HistoryStatus::Stale, false, Some(error)),
            None => (HistoryStatus::Ready, true, None),
        }
    } else if !empty {
        (
            HistoryStatus::Partial,
            false,
            Some(last_error.unwrap_or(not_loaded)),
        )
    } else if let Some(error) = last_error {
        (HistoryStatus::Unavailable, false, Some(error))
    } else {
        (HistoryStatus::Loading, false, None)
    };

    // A row the writer refused or could not store is a hole in the stored
    // conversation, and the reader is told about it. A status that says
    // "complete" over a known hole is the one lie this must not tell, so the
    // mark makes the view stale and its reason replaces the softer ones.
    match unsaved {
        Some(unsaved) => (
            match status {
                HistoryStatus::Ready | HistoryStatus::Stale => HistoryStatus::Stale,
                other => other,
            },
            false,
            Some(unsaved),
        ),
        None => (status, complete, reason),
    }
}

/// What a read of a thread's conversation should answer.
///
/// A read is more than a lookup: it is the only thing that knows somebody
/// wants the conversation, so it is also where "this needs loading" is
/// decided.
#[derive(Debug)]
pub enum ThreadHistoryRead {
    /// Serve these rows, whatever their status.
    Serve(CacheView),
    /// Nothing to serve yet, and the caller asks again. `reason` says why the
    /// wait is not over when that is not simply "the load is running" — a run
    /// holding the thread, for instance.
    Loading { reason: Option<String> },
    /// Nothing to serve and nothing that can be loaded, with the reason.
    Unavailable(String),
}

impl AppState {
    /// Reads a thread's conversation, asking for a load when one is needed.
    ///
    /// The triggers are the ones a read can act on:
    ///
    /// * **nothing cached** — ask for a load and answer `loading`, because a
    ///   load can take as long as an agent's cold start;
    /// * **an overlay with no baseline** — the thread has said something since
    ///   this server started but nobody has loaded its past. Load it, *unless a
    ///   run is in flight*: the run owns the session, the worker serializes a
    ///   load against a prompt, and what the overlay holds is worth showing in
    ///   the meantime. After a restart no run is in flight, which is exactly
    ///   the case this exists for;
    /// * **stale** — serve what is cached and load a newer view behind it;
    /// * **ready** — serve it. A cache hit costs no ACP call;
    /// * **unavailable** — serve the reason. A retry is an explicit act, not
    ///   something every poll does: a deleted session would otherwise be asked
    ///   for again on each read.
    pub fn read_thread_history(&self, thread_id: &ThreadId) -> ThreadHistoryRead {
        let view = match self.stored_view(thread_id) {
            Ok(view) => view,
            Err(error) => {
                return ThreadHistoryRead::Unavailable(format!(
                    "the stored conversation could not be read: {error}"
                ))
            }
        };
        match view.status {
            // Nothing stored and nothing on screen: this read is what asks for
            // the conversation, and the answer is `loading` because a load can
            // take as long as an agent's cold start.
            HistoryStatus::Loading => match self.start_thread_history_load(thread_id) {
                Ok(()) | Err(HistoryUnavailable::Busy) => {
                    ThreadHistoryRead::Loading { reason: None }
                }
                Err(HistoryUnavailable::RunInFlight) => ThreadHistoryRead::Loading {
                    reason: Some(HistoryUnavailable::RunInFlight.to_string()),
                },
                Err(error) => ThreadHistoryRead::Unavailable(error.to_string()),
            },
            // Rows are worth showing while a newer view loads. Whether a load
            // may start is the loader's decision, not the reader's: it is the
            // same decision as the claim, and splitting it across two steps is
            // what let a run slip in between them.
            //
            // A failure waits out its backoff first: a page that polls is not a
            // reason to ask an agent that just said no, over and over.
            HistoryStatus::Partial | HistoryStatus::Stale => {
                self.retry_history_load_if_due(thread_id, view.local_complete);
                ThreadHistoryRead::Serve(view)
            }
            // A conversation with nothing in it whose load failed is the one
            // state a read cannot improve by showing what it has — there is
            // nothing to show. It is still retried, on the same terms: a thread
            // must not need a person to type something before it can recover.
            HistoryStatus::Unavailable => {
                self.retry_history_load_if_due(thread_id, view.local_complete);
                ThreadHistoryRead::Serve(view)
            }
            HistoryStatus::Ready => ThreadHistoryRead::Serve(view),
        }
    }

    /// The conversation's expensive values, without the rows.
    ///
    /// Everything [`AppState::stored_view`] needs to derive a status, read as
    /// an indexed last sequence plus an in-memory look at the overlay. A
    /// reader holding a recent projection asks this to decide whether the
    /// projection is still current, and never pays to read the conversation a
    /// second time for an answer it already has.
    pub(crate) fn history_meta(
        &self,
        thread_id: &ThreadId,
    ) -> Result<HistoryMeta, crate::store::StoreError> {
        // The header and the last sequence are read under one guard so a
        // rebuild cannot land between them and leave the two describing
        // different conversations.
        let (history, stored_last_seq) = {
            let store = self.store();
            let history = store.history(thread_id)?;
            let stored_last_seq = store.stored_last_seq(thread_id)?;
            (history, stored_last_seq)
        };
        let last_seq = stored_last_seq.max(self.history.last_seq(thread_id));
        let (status, complete, reason) = derive_history_status(
            history.as_ref(),
            last_seq == 0,
            self.history.is_loading(thread_id),
            self.store_writer().unsaved(thread_id),
        );
        Ok(HistoryMeta {
            generation: history.as_ref().map(|stored| stored.revision).unwrap_or(0),
            status,
            complete,
            local_complete: history.as_ref().is_some_and(|stored| stored.local_complete),
            reason,
            last_seq,
        })
    }

    /// The retry a read owes a conversation that is not complete yet.
    ///
    /// The load decision belongs to the loader, not the reader — starting one
    /// is the same decision as claiming it — so a reader with rows to serve
    /// only asks whether the wait since the last failure is over.
    pub(crate) fn retry_history_load_if_due(&self, thread_id: &ThreadId, local_complete: bool) {
        if !local_complete && self.history.may_retry(thread_id, HISTORY_RETRY_BACKOFF) {
            let _ = self.start_thread_history_load(thread_id);
        }
    }

    /// The conversation as it stands: what the store holds, plus what has been
    /// published and not written yet.
    ///
    /// One source per read: the stored rows are the conversation, and the
    /// overlay supplies only the tail that has not reached the disk. They are
    /// matched by sequence and the stored row wins, so a row that is in both
    /// places is not shown twice.
    ///
    /// The status is *derived* rather than remembered — what is stored, whether
    /// a load is in flight, and why the last one failed — which is what lets it
    /// survive a restart that empties the in-memory overlay.
    pub(crate) fn stored_view(
        &self,
        thread_id: &ThreadId,
    ) -> Result<CacheView, crate::store::StoreError> {
        let (instance, history, rows) = {
            let store = self.store();
            let history = store.history(thread_id)?;
            let rows: Vec<CachedRow> = store
                .rows(thread_id)?
                .into_iter()
                .map(CachedRow::from)
                .collect();
            let stored: std::collections::HashSet<u64> = rows.iter().map(|row| row.seq).collect();
            let mut merged = rows;
            if let Some(overlay) = self.history.rows(thread_id) {
                merged.extend(overlay.into_iter().filter(|row| !stored.contains(&row.seq)));
            }
            merged.sort_by_key(|row| row.seq);
            (store.instance().to_owned(), history, merged)
        };

        let local_complete = history.as_ref().is_some_and(|stored| stored.local_complete);
        let revision = history.as_ref().map(|stored| stored.revision).unwrap_or(0);
        let (status, complete, reason) = derive_history_status(
            history.as_ref(),
            rows.is_empty(),
            self.history.is_loading(thread_id),
            self.store_writer().unsaved(thread_id),
        );

        Ok(CacheView {
            instance,
            generation: revision,
            status,
            complete,
            local_complete,
            reason,
            rows,
        })
    }

    /// Re-reads a thread's conversation from the agent that owns it.
    ///
    /// A read is a poll: it serves what is stored and, when something suggests
    /// the stored conversation may not be current, asks for a load behind it.
    /// This is the explicit ask. It is the only thing that can notice a session
    /// that moved on somewhere this server cannot see — a terminal running the
    /// agent against the same session, another machine — and nothing here polls
    /// for that, because noticing would mean asking a machine on every page.
    ///
    /// It is also the way back from a failure before its backoff has passed.
    pub fn refresh_thread_history(&self, thread_id: &ThreadId) -> ThreadHistoryRead {
        // Explicit means now: this does not wait out a failed attempt's backoff.
        self.history.clear_sync_failure(thread_id);
        match self.start_thread_history_load(thread_id) {
            // Already loading, at the concurrency bound, or behind a run that
            // owns the session: what is stored is the answer either way, and
            // the status says so.
            Ok(()) | Err(HistoryUnavailable::Busy) | Err(HistoryUnavailable::RunInFlight) => {}
            Err(error) => return ThreadHistoryRead::Unavailable(error.to_string()),
        }
        match self.stored_view(thread_id) {
            Ok(view) => ThreadHistoryRead::Serve(view),
            Err(error) => ThreadHistoryRead::Unavailable(error.to_string()),
        }
    }

    /// Whether a run owns this thread right now.
    ///
    /// A run in flight is what a history load must not race: it holds the
    /// provider session, and a load started behind it would either be refused
    /// or wait for the turn to end. The overlay is the honest answer until then.
    fn thread_has_run_in_flight(&self, thread_id: &ThreadId) -> bool {
        // The claim is what a dispatch takes before it records the run on the
        // thread, so it is the earlier signal; the thread's own `active_run_id`
        // covers a run whose record is already gone.
        self.runs.claims_thread(thread_id)
            || self
                .registry
                .thread(thread_id)
                .is_some_and(|thread| thread.active_run_id.is_some())
    }

    /// The conversation for a thread, loading it from its host if needed.
    pub async fn ensure_thread_history(
        &self,
        thread_id: &ThreadId,
    ) -> Result<CacheView, HistoryUnavailable> {
        if let Some(view) = self.complete_view(thread_id) {
            return Ok(view);
        }
        let binding = self
            .thread_cache_binding(thread_id)
            .ok_or(HistoryUnavailable::NoBinding)?;
        let operation = self.history_operation(thread_id, &binding)?;
        let host_id = binding.host_id.clone();
        let state = self.clone();
        self.ensure_history(thread_id, binding, move || {
            let state = state.clone();
            let operation = operation.clone();
            async move {
                state
                    .load_thread_history(&host_id, operation, HISTORY_LOAD_DEADLINE)
                    .await
            }
        })
        .await
    }

    /// The whole flow, with the load itself supplied by the caller.
    ///
    /// Taking the load as an argument is what makes the interesting part —
    /// who loads, who waits, and what a failure leaves behind — testable
    /// without a host.
    pub async fn ensure_history<L, Fut>(
        &self,
        thread_id: &ThreadId,
        binding: CacheBinding,
        load: L,
    ) -> Result<CacheView, HistoryUnavailable>
    where
        L: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<Vec<ProviderEvent>, HistoryTransportError>>,
    {
        // A complete cached conversation needs no load. This is inside the
        // shared flow rather than at the caller, so every entry point gets it
        // and a refresh (a `Stale` view) still loads.
        if let Some(view) = self.complete_view(thread_id) {
            return Ok(view);
        }
        match self.history.begin_load(thread_id, &binding) {
            LoadTicket::Leader => {
                // A cached conversation stays visible while it is replaced; it
                // is marked stale, not blanked, because the user may be
                // reading it right now.
                let mark = self.seqs().position(thread_id);
                let outcome = load().await;
                self.settle_history_load(thread_id, binding, mark, outcome)
                    .await
            }
            LoadTicket::Follower => self.await_leader(thread_id).await,
            LoadTicket::Refused => Err(HistoryUnavailable::Busy),
        }
    }

    /// Starts a thread's load without waiting for it.
    ///
    /// A load can take as long as an agent's cold start, so a page fetch must
    /// not hold a connection for one. The caller is told `loading` and retries,
    /// and every other caller joins the same load rather than starting another.
    ///
    /// The request is built before the claim is taken, so a thread whose bound
    /// agent cannot be resolved fails here and leaves nothing claimed.
    pub fn start_thread_history_load(
        &self,
        thread_id: &ThreadId,
    ) -> Result<(), HistoryUnavailable> {
        let binding = self
            .thread_cache_binding(thread_id)
            .ok_or(HistoryUnavailable::NoBinding)?;
        let operation = self.history_operation(thread_id, &binding)?;
        // Whether a run owns the thread and whether this load may start are one
        // decision, taken under the lock a dispatch takes to claim the thread:
        // otherwise a run can be claimed between the check and the claim, and
        // the load it should have yielded to starts anyway. The guard is
        // released before the load runs — a load may take a minute, and it must
        // not hold the lifecycle lock while it does.
        {
            let _lifecycle = self.runs.lifecycle_lock();
            if self.thread_has_run_in_flight(thread_id) {
                return Err(HistoryUnavailable::RunInFlight);
            }
        }
        match self.history.begin_load(thread_id, &binding) {
            LoadTicket::Leader => {
                // Where the numbering stood when the load starts. A row
                // reserved after this means the replay is not the whole
                // conversation any more; see `settle_history_load`.
                let mark = self.seqs().position(thread_id);
                let state = self.clone();
                let thread_id = thread_id.clone();
                tokio::spawn(async move {
                    let outcome = state
                        .load_thread_history(&binding.host_id, operation, HISTORY_LOAD_DEADLINE)
                        .await;
                    let _ = state
                        .settle_history_load(&thread_id, binding, mark, outcome)
                        .await;
                });
                Ok(())
            }
            // Already being loaded: this caller waits by retrying, which is
            // what a page fetch with a `loading` status does.
            LoadTicket::Follower => Ok(()),
            LoadTicket::Refused => Err(HistoryUnavailable::Busy),
        }
    }

    /// Installs the outcome of one load and wakes whoever is waiting on it.
    ///
    /// The single place a load's result reaches the cache, so a failure can
    /// never leave a partial conversation behind and a claim can never be held
    /// past the load that took it.
    async fn settle_history_load(
        &self,
        thread_id: &ThreadId,
        binding: CacheBinding,
        mark: u64,
        outcome: Result<Vec<ProviderEvent>, HistoryTransportError>,
    ) -> Result<CacheView, HistoryUnavailable> {
        match outcome {
            Ok(events) => {
                // A replay is the whole conversation as of the moment it was
                // collected. If the thread has said something since, this one is
                // not that whole any more, and installing it would put a
                // baseline under rows that came after it. The result is dropped
                // instead, and the next read after the run ends loads again.
                if self.seqs().position(thread_id) != mark {
                    let reason = "the thread changed while its history was loading";
                    if let Err(error) = self.store().record_sync_failure(thread_id, reason) {
                        eprintln!("loom-server: recording a refused rebuild failed: {error}");
                    }
                    self.history.finish_load(thread_id);
                    self.history.mark_sync_failed(thread_id);
                    self.history_waits.wake(thread_id);
                    return Err(HistoryUnavailable::Incomplete(reason.to_owned()));
                }
                let first = self.seqs().reserve(thread_id, events.len());
                let synced_at_ms = loom_relay::now_ms();
                match self.store().replace_replayed(
                    thread_id,
                    &binding,
                    first,
                    &events,
                    synced_at_ms,
                ) {
                    Ok(_revision) => {
                        // Which conversation a thread is now is the store's
                        // answer, and rows that arrive next are checked against
                        // it rather than against what was known before.
                        self.history.adopt_binding(thread_id, &binding);
                        self.history.clear_sync_failure(thread_id);
                        // A rebuilt baseline replaces what is stored, holes
                        // included, so the thread is no longer unsaved.
                        self.store_writer().clear_unsaved(thread_id);
                    }
                    Err(error) => {
                        let reason = format!("the conversation could not be stored: {error}");
                        if let Err(record) = self.store().record_sync_failure(thread_id, &reason) {
                            eprintln!("loom-server: recording a failed rebuild failed: {record}");
                        }
                        self.history.finish_load(thread_id);
                        self.history.mark_sync_failed(thread_id);
                        self.history_waits.wake(thread_id);
                        return Err(HistoryUnavailable::Incomplete(reason));
                    }
                }
            }
            Err(HistoryTransportError::Failed { code, message: _ }) if code == "unsupported" => {
                if self.seqs().position(thread_id) != mark {
                    let reason = "the thread changed while its history was loading";
                    if let Err(error) = self.store().record_sync_failure(thread_id, reason) {
                        eprintln!("loom-server: recording a refused local history fallback failed: {error}");
                    }
                    self.history.finish_load(thread_id);
                    self.history.mark_sync_failed(thread_id);
                    self.history_waits.wake(thread_id);
                    return Err(HistoryUnavailable::Incomplete(reason.to_owned()));
                }

                let already_local = match self.store().history(thread_id) {
                    Ok(history) => history.is_some_and(|stored| stored.local_complete),
                    Err(error) => {
                        self.history.finish_load(thread_id);
                        self.history_waits.wake(thread_id);
                        return Err(HistoryUnavailable::Incomplete(error.to_string()));
                    }
                };
                if already_local {
                    self.history.finish_load(thread_id);
                    self.history_waits.wake(thread_id);
                    return self.complete_view_result(thread_id);
                }

                match self
                    .store_writer()
                    .mark_locally_complete(thread_id, &binding)
                    .await
                {
                    Ok(()) => {
                        // The agent cannot replay, so the ordered durable rows
                        // Loom observed are the history source for this thread.
                        self.history.adopt_binding(thread_id, &binding);
                        self.history.clear_sync_failure(thread_id);
                    }
                    Err(error) => {
                        let reason = format!(
                            "the agent cannot replay history and the local conversation could not be certified: {error}"
                        );
                        if let Err(record) = self.store().record_sync_failure(thread_id, &reason) {
                            eprintln!("loom-server: recording a local history fallback failure failed: {record}");
                        }
                        self.history.finish_load(thread_id);
                        self.history.mark_sync_failed(thread_id);
                        self.history_waits.wake(thread_id);
                        return Err(HistoryUnavailable::Incomplete(reason));
                    }
                }
            }
            Err(error) => {
                let reason = error.to_string();
                if let Err(record) = self.store().record_sync_failure(thread_id, &reason) {
                    eprintln!("loom-server: recording a failed load failed: {record}");
                }
                self.history.finish_load(thread_id);
                self.history.mark_sync_failed(thread_id);
                self.history_waits.wake(thread_id);
                return Err(HistoryUnavailable::from(error));
            }
        }
        self.history.finish_load(thread_id);
        self.history_waits.wake(thread_id);
        self.complete_view_result(thread_id)
    }

    /// Waits for the load another caller started, then reads what it produced.
    async fn await_leader(&self, thread_id: &ThreadId) -> Result<CacheView, HistoryUnavailable> {
        let signal = self.history_waits.signal(thread_id);
        let waiter = signal.notified();
        tokio::pin!(waiter);
        // Registered before the check, so a leader that finishes between the
        // two lines still wakes this waiter rather than leaving it parked.
        waiter.as_mut().enable();
        if !self.history.is_loading(thread_id) {
            return self.complete_view_result(thread_id);
        }
        if tokio::time::timeout(HISTORY_LOAD_DEADLINE, waiter)
            .await
            .is_err()
        {
            return Err(HistoryUnavailable::Timeout);
        }
        self.complete_view_result(thread_id)
    }

    /// The conversation when it is complete and nothing else is needed.
    fn complete_view(&self, thread_id: &ThreadId) -> Option<CacheView> {
        self.stored_view(thread_id)
            .ok()
            .filter(|view| view.complete)
    }

    /// The conversation, or the reason there is not a usable one.
    fn complete_view_result(&self, thread_id: &ThreadId) -> Result<CacheView, HistoryUnavailable> {
        match self.stored_view(thread_id) {
            Ok(view) if view.complete => Ok(view),
            Ok(view) => Err(HistoryUnavailable::Incomplete(
                view.reason
                    .unwrap_or_else(|| "the conversation could not be loaded".to_owned()),
            )),
            Err(error) => Err(HistoryUnavailable::Incomplete(error.to_string())),
        }
    }

    /// What this thread's conversation is bound to, when that is provable.
    ///
    /// A missing host is the same answer as a missing binding: the session id
    /// names a file on one machine's disk, and without knowing which machine
    /// there is nothing to ask.
    pub fn thread_cache_binding(&self, thread_id: &ThreadId) -> Option<CacheBinding> {
        let thread = self.registry.thread(thread_id)?;
        let binding = thread.provider_session_binding.clone()?;
        let provider_session_id = thread.provider_session_id.clone()?;
        Some(CacheBinding {
            host_id: binding.host_id?,
            agent: binding.agent,
            provider_session_id,
            cwd: binding.cwd,
        })
    }

    /// The load request for a binding, resolved against the bound host.
    fn history_operation(
        &self,
        thread_id: &ThreadId,
        binding: &CacheBinding,
    ) -> Result<HostRpcOperation, HistoryUnavailable> {
        let provider = self
            .provider_spec_for_host(&binding.host_id, &binding.agent)
            .or_else(|| {
                let default = self.provider_spec();
                (default.name == binding.agent).then_some(default)
            })
            .ok_or_else(|| HistoryUnavailable::UnknownProvider {
                host_id: binding.host_id.clone(),
                agent: binding.agent.clone(),
            })?;
        Ok(HostRpcOperation::LoadHistory {
            thread_id: thread_id.clone(),
            provider,
            provider_session_id: binding.provider_session_id.clone(),
            cwd: binding.cwd.clone(),
            max_batch_bytes: HISTORY_MAX_BATCH_BYTES,
            max_total_bytes: HISTORY_MAX_TOTAL_BYTES,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::AppConfig;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn test_state() -> AppState {
        AppState::build(AppConfig {
            reconcile_interval: Duration::ZERO,
            ..AppConfig::default()
        })
        .unwrap()
    }

    /// Writes a loaded baseline where a load writes it: the store. The
    /// in-memory overlay is only ever the tail that has not been written.
    fn seed_baseline(
        state: &AppState,
        thread: &ThreadId,
        binding: &CacheBinding,
        events: &[ProviderEvent],
    ) -> u64 {
        let first = state.seqs().reserve(thread, events.len());
        state
            .store()
            .replace_replayed(thread, binding, first, events, loom_relay::now_ms())
            .expect("the baseline is stored")
    }

    fn cache_binding() -> CacheBinding {
        CacheBinding {
            host_id: HostId::mint(),
            agent: "pi".to_owned(),
            provider_session_id: "acp-session-1".to_owned(),
            cwd: "/srv/project".to_owned(),
        }
    }

    fn identity() -> ProviderEvent {
        ProviderEvent::ThreadIdentity {
            provider_thread_id: "acp-session-1".into(),
        }
    }

    /// Assert the cheap facts equal the full view they stand in for.
    fn assert_meta_matches_view(meta: &HistoryMeta, view: &CacheView) {
        assert_eq!(meta.generation, view.generation);
        assert_eq!(meta.status, view.status);
        assert_eq!(meta.complete, view.complete);
        assert_eq!(meta.local_complete, view.local_complete);
        assert_eq!(meta.reason, view.reason);
        assert_eq!(
            meta.last_seq,
            view.rows.last().map(|row| row.seq).unwrap_or(0)
        );
    }

    /// The metadata a cached projection is validated against must say exactly
    /// what the full read says: a reader that trusted a different status would
    /// tell a different story than the one serving the rows.
    #[tokio::test]
    async fn metadata_says_what_the_view_says() {
        let state = test_state();
        let thread = ThreadId::mint();
        let binding = cache_binding();

        // Nothing stored and nothing on screen: there is nothing to show yet.
        let view = state.stored_view(&thread).unwrap();
        let meta = state.history_meta(&thread).unwrap();
        assert_meta_matches_view(&meta, &view);
        assert_eq!(meta.status, HistoryStatus::Loading);
        assert_eq!(meta.last_seq, 0);

        // A stored baseline: ready and complete.
        seed_baseline(&state, &thread, &binding, &[identity()]);
        let view = state.stored_view(&thread).unwrap();
        let meta = state.history_meta(&thread).unwrap();
        assert_eq!(meta.status, HistoryStatus::Ready);
        assert!(meta.complete);
        assert_meta_matches_view(&meta, &view);

        // An unwritten tail is part of the conversation and moves the sequence
        // a projection is keyed by.
        let seq = state.seqs().reserve(&thread, 1);
        state.history.append_live(
            &thread,
            Some(&binding),
            seq,
            crate::history_cache::RowSource::Message { at_ms: 1 },
            identity(),
        );
        let view = state.stored_view(&thread).unwrap();
        let meta = state.history_meta(&thread).unwrap();
        assert_meta_matches_view(&meta, &view);
        assert_eq!(meta.last_seq, seq, "the overlay is part of the row set");
        state.shutdown().unwrap();
    }

    #[tokio::test]
    async fn a_cache_miss_loads_and_installs_the_baseline() {
        let state = test_state();
        let thread = ThreadId::mint();
        let view = state
            .ensure_history(&thread, cache_binding(), || async {
                Ok(vec![identity(), identity()])
            })
            .await
            .expect("the loader produced a conversation");

        assert_eq!(view.status, HistoryStatus::Ready);
        assert!(view.complete);
        assert_eq!(
            view.rows.iter().map(|row| row.seq).collect::<Vec<_>>(),
            vec![1, 2]
        );
        state.shutdown().unwrap();
    }

    #[tokio::test]
    async fn a_provider_without_history_replay_uses_ordered_local_rows() {
        let dir = tempfile::tempdir().unwrap();
        let config = AppConfig {
            backend_path: Some(dir.path().to_path_buf()),
            reconcile_interval: Duration::ZERO,
            entity_write_interval: Duration::ZERO,
            ..AppConfig::default()
        };
        let state = AppState::build(config.clone()).unwrap();
        let thread = ThreadId::mint();
        let binding = cache_binding();
        let seq = state.seqs().reserve(&thread, 1);
        let local_prompt = ProviderEvent::ItemStarted {
            item: loom_domain::ThreadEventItem::UserMessage {
                id: "message-1".to_owned(),
                content: vec![loom_domain::UserContent::Text {
                    text: "hello".to_owned(),
                }],
                client_request_id: None,
                parent_tool_call_id: None,
            },
            provider_thread_id: String::new(),
        };
        assert!(state.store_writer().enqueue(
            &thread,
            seq,
            crate::history_cache::RowSource::Message { at_ms: 1 },
            local_prompt,
        ));

        let view = state
            .ensure_history(&thread, binding.clone(), || async {
                Err(HistoryTransportError::Failed {
                    code: "unsupported".to_owned(),
                    message: "session/load is not supported".to_owned(),
                })
            })
            .await
            .expect("the server's complete local history is usable");

        assert_eq!(view.status, HistoryStatus::Ready);
        assert!(view.complete);
        assert!(view.local_complete);
        assert_eq!(view.rows.len(), 1);
        assert_eq!(
            state.store().history(&thread).unwrap().unwrap().binding,
            Some(binding)
        );
        assert_eq!(state.store().rows(&thread).unwrap().len(), 1);

        let loads = AtomicUsize::new(0);
        let loaded = state
            .ensure_history(&thread, cache_binding(), || {
                loads.fetch_add(1, Ordering::SeqCst);
                async { Ok(vec![identity()]) }
            })
            .await
            .expect("the local history is already complete");
        assert_eq!(loaded.rows.len(), 1);
        assert_eq!(loads.load(Ordering::SeqCst), 0);
        state.shutdown().unwrap();
        drop(state);

        let reopened = AppState::build(config).unwrap();
        let restored = reopened.stored_view(&thread).unwrap();
        assert_eq!(restored.status, HistoryStatus::Ready);
        assert!(restored.complete);
        assert!(restored.local_complete);
        assert_eq!(restored.rows.len(), 1);
        reopened
            .store()
            .mark_stored_history_behind("the server stopped unexpectedly")
            .unwrap();
        match reopened.read_thread_history(&thread) {
            ThreadHistoryRead::Serve(view) => {
                assert_eq!(view.status, HistoryStatus::Stale);
                assert!(view.local_complete);
            }
            other => panic!("expected the locally stored rows to remain visible: {other:?}"),
        }
        assert!(
            !reopened.history.is_loading(&thread),
            "a resume-only agent cannot refresh local history through ACP"
        );
        reopened.shutdown().unwrap();
    }

    #[tokio::test]
    async fn an_unclean_local_tail_cannot_be_marked_complete_by_an_unsupported_agent() {
        let state = test_state();
        let thread = ThreadId::mint();
        let binding = cache_binding();
        let seq = state.seqs().reserve(&thread, 1);
        let prompt = ProviderEvent::ItemStarted {
            item: loom_domain::ThreadEventItem::UserMessage {
                id: "message-unclean".to_owned(),
                content: vec![loom_domain::UserContent::Text {
                    text: "possibly incomplete".to_owned(),
                }],
                client_request_id: None,
                parent_tool_call_id: None,
            },
            provider_thread_id: String::new(),
        };
        assert!(state.store_writer().enqueue(
            &thread,
            seq,
            crate::history_cache::RowSource::Message { at_ms: 1 },
            prompt,
        ));
        assert!(state
            .store_writer()
            .wait_for_writes(1, Duration::from_secs(2)));
        state
            .store()
            .mark_stored_history_behind("the server stopped without finishing")
            .unwrap();

        let failure = state
            .ensure_history(&thread, binding, || async {
                Err(HistoryTransportError::Failed {
                    code: "unsupported".to_owned(),
                    message: "session/load is not supported".to_owned(),
                })
            })
            .await
            .expect_err("a partial local tail cannot be certified without replay");

        assert!(matches!(failure, HistoryUnavailable::Incomplete(_)));
        let history = state.store().history(&thread).unwrap().unwrap();
        assert!(!history.local_complete);
        assert!(history.local_uncertain);
        assert!(history.last_error.is_some());
        assert_eq!(
            state.stored_view(&thread).unwrap().status,
            HistoryStatus::Partial
        );
        state.shutdown().unwrap();
    }

    #[tokio::test]
    async fn a_complete_conversation_is_not_loaded_again() {
        let state = test_state();
        let thread = ThreadId::mint();
        let binding = cache_binding();
        seed_baseline(&state, &thread, &binding, &[identity()]);
        let loads = Arc::new(AtomicUsize::new(0));

        let loads_for_loader = Arc::clone(&loads);
        let view = state
            .ensure_history(&thread, binding, move || {
                loads_for_loader.fetch_add(1, Ordering::SeqCst);
                async { Ok(vec![identity()]) }
            })
            .await
            .expect("the cached conversation is returned");

        assert_eq!(view.rows.len(), 1);
        assert_eq!(loads.load(Ordering::SeqCst), 0, "no load was needed");
        state.shutdown().unwrap();
    }

    /// A rebuild moves the durable revision, and a client holding a cursor from
    /// before it is told to refetch. The revision lives with the file, so this
    /// holds across a restart too.
    #[tokio::test]
    async fn a_rebuild_moves_the_durable_revision() {
        let state = test_state();
        let thread = ThreadId::mint();
        let binding = cache_binding();
        seed_baseline(&state, &thread, &binding, &[identity()]);

        let first = state.stored_view(&thread).unwrap();
        assert_eq!(first.status, HistoryStatus::Ready);
        assert!(
            first.complete,
            "a stored baseline is the whole conversation"
        );
        assert_eq!(first.rows.len(), 1);
        assert_eq!(first.generation, 1);

        // A second load of the same session replaces the replay.
        seed_baseline(&state, &thread, &binding, &[identity(), identity()]);
        let second = state.stored_view(&thread).unwrap();
        assert!(
            second.generation > first.generation,
            "a rebuild must not reuse a revision"
        );
        assert_eq!(second.rows.len(), 2);
        assert_eq!(
            second.instance, first.instance,
            "the numbering is the same store's, only its revision moved"
        );
        state.shutdown().unwrap();
    }

    /// A row the store could not write is reported to a reader, not hidden
    /// behind a base that still calls itself complete.
    #[tokio::test]
    async fn a_row_the_store_refused_is_reported_by_the_read() {
        let state = test_state();
        let (thread, created) = state
            .registry
            .create_thread(
                Some(state.registry.personal_project_id()),
                Some("unsaved".into()),
                None,
                loom_relay::now_ms(),
            )
            .unwrap();
        state.publish_domain_event(&created).unwrap();
        seed_baseline(&state, &thread.id, &cache_binding(), &[identity()]);
        assert!(state.stored_view(&thread.id).unwrap().complete);

        // A writer that has stopped accepting rows: what a disk in trouble
        // looks like from the publish path, with reads still working.
        state.store_writer().flush().unwrap();
        for event in state
            .registry
            .post_message(
                &thread.id,
                loom_domain::MessageRole::User,
                "this row has nowhere to go".into(),
                loom_relay::now_ms(),
            )
            .unwrap()
        {
            state.publish_domain_event(&event).unwrap();
        }
        assert!(
            state.store_writer().unsaved(&thread.id).is_some(),
            "the refused row marks the thread"
        );

        let view = state.stored_view(&thread.id).unwrap();
        assert!(
            !view.complete,
            "a conversation with a known hole is not complete: {view:?}"
        );
        assert_eq!(view.status, HistoryStatus::Stale);
        assert!(
            view.reason
                .as_deref()
                .is_some_and(|reason| reason.contains("not stored")),
            "the reason says the row was not stored: {view:?}"
        );
        // The row itself is still on screen while it is only in memory.
        assert!(
            view.rows
                .iter()
                .any(|row| matches!(row.source, crate::history_cache::RowSource::Message { .. })),
            "the unpublished row is still served: {view:?}"
        );
    }

    /// A refresh is the explicit ask: it starts a load even when the stored
    /// conversation is complete, because the thing it exists for is a session
    /// that moved on somewhere this server cannot see.
    #[tokio::test]
    async fn a_refresh_asks_for_the_conversation_again() {
        let state = test_state();
        let (thread, _) = state
            .registry
            .create_thread(
                Some(state.registry.personal_project_id()),
                Some("refresh".into()),
                None,
                loom_relay::now_ms(),
            )
            .unwrap();
        state
            .registry
            .set_provider_session_id(
                &thread.id,
                "acp-session-1",
                Some(
                    loom_domain::ProviderSessionBinding::new("pi", "/srv/project")
                        .on_host(HostId::mint()),
                ),
                loom_relay::now_ms(),
            )
            .unwrap();
        seed_baseline(&state, &thread.id, &cache_binding(), &[identity()]);

        // A complete conversation is served without a load.
        assert!(matches!(
            state.read_thread_history(&thread.id),
            ThreadHistoryRead::Serve(ref view) if view.status == HistoryStatus::Ready
        ));
        assert!(!state.history.is_loading(&thread.id));

        // The explicit ask does not take that for an answer.
        match state.refresh_thread_history(&thread.id) {
            ThreadHistoryRead::Serve(view) => assert_eq!(
                view.status,
                HistoryStatus::Stale,
                "a load is now behind what is stored: {view:?}"
            ),
            other => panic!("a refresh serves what is stored, got {other:?}"),
        }
        assert!(
            state.history.is_loading(&thread.id),
            "the refresh started a load"
        );
        state.shutdown().unwrap();
    }

    /// A failed load is not retried by the very next read: the wait is what
    /// keeps a page that polls from asking an agent that is down over and over.
    /// The thread here is fully loadable, so the only thing holding the second
    /// attempt back is the backoff.
    #[tokio::test]
    async fn a_failed_load_waits_before_the_next_read_retries_it() {
        let state = test_state();
        let (thread, _) = state
            .registry
            .create_thread(
                Some(state.registry.personal_project_id()),
                Some("offline".into()),
                None,
                loom_relay::now_ms(),
            )
            .unwrap();
        state
            .registry
            .set_provider_session_id(
                &thread.id,
                "acp-session-1",
                Some(
                    loom_domain::ProviderSessionBinding::new("pi", "/srv/project")
                        .on_host(HostId::mint()),
                ),
                loom_relay::now_ms(),
            )
            .unwrap();

        let failure = state
            .ensure_history(&thread.id, cache_binding(), || async {
                Err(HistoryTransportError::Failed {
                    code: "offline".to_owned(),
                    message: "the agent is not there".to_owned(),
                })
            })
            .await
            .expect_err("the load failed");
        assert!(
            matches!(failure, HistoryUnavailable::Host { .. }),
            "{failure:?}"
        );

        match state.read_thread_history(&thread.id) {
            ThreadHistoryRead::Serve(view) => {
                assert_eq!(view.status, HistoryStatus::Unavailable, "{view:?}");
                assert!(
                    view.reason
                        .as_deref()
                        .is_some_and(|reason| reason.contains("not there")),
                    "the reason it failed is what the reader is told: {view:?}"
                );
            }
            other => panic!("expected the failure to be served, got {other:?}"),
        }
        assert!(
            !state.history.is_loading(&thread.id),
            "the retry waits out its backoff rather than starting on this read"
        );
        state.shutdown().unwrap();
    }

    /// A thread that talks while its replay is in flight has outgrown that
    /// replay: installing it would drop what just arrived, so the result is
    /// discarded and the overlay keeps its rows.
    #[tokio::test]
    async fn a_baseline_the_thread_outgrew_is_dropped_instead_of_installed() {
        let state = test_state();
        let thread = ThreadId::mint();
        let binding = cache_binding();
        let loading = state.clone();
        let loading_thread = thread.clone();

        let failure = state
            .ensure_history(&thread, binding.clone(), move || async move {
                // The thread says something while the load is in flight.
                let seq = loading.seqs().reserve(&loading_thread, 1);
                loading.history.append_live(
                    &loading_thread,
                    Some(&binding),
                    seq,
                    crate::history_cache::RowSource::Message { at_ms: 1 },
                    identity(),
                );
                Ok(vec![identity(), identity(), identity()])
            })
            .await
            .expect_err("a replay the thread outgrew is not a conversation");

        assert!(
            matches!(failure, HistoryUnavailable::Incomplete(ref message)
                if message.contains("changed while its history was loading")),
            "{failure:?}"
        );
        let view = state.stored_view(&thread).unwrap();
        assert_eq!(
            view.rows.len(),
            1,
            "the live event survived the refused install"
        );
        assert_ne!(view.status, HistoryStatus::Ready);
        state.shutdown().unwrap();
    }

    /// A run in flight is what a load yields to, and the check is part of the
    /// claim: a read that finds a run says *why* it is waiting, and a load
    /// that arrives while one runs claims nothing.
    #[tokio::test]
    async fn a_load_yields_to_a_run_in_flight() {
        let state = test_state();
        let (thread, _) = state
            .registry
            .create_thread(
                Some(state.registry.personal_project_id()),
                Some("busy".into()),
                None,
                loom_relay::now_ms(),
            )
            .unwrap();
        state
            .registry
            .set_provider_session_id(
                &thread.id,
                "acp-session-1",
                Some(
                    loom_domain::ProviderSessionBinding::new("pi", "/srv/project")
                        .on_host(HostId::mint()),
                ),
                loom_relay::now_ms(),
            )
            .unwrap();
        let run_id = loom_domain::RunId::mint();
        state
            .runs
            .claim_thread(&thread.id, run_id.clone())
            .expect("the thread is free");

        // Nothing cached yet: the answer is "waiting for the run", not
        // "unavailable" — the conversation is being written right now.
        match state.read_thread_history(&thread.id) {
            ThreadHistoryRead::Loading {
                reason: Some(reason),
            } => {
                assert!(reason.contains("run in flight"), "{reason}");
            }
            other => panic!("expected a waiting read, got {other:?}"),
        }
        assert!(
            !state.history.is_loading(&thread.id),
            "a refused load must not claim anything"
        );

        // An overlay behind a run is served as it is, and still starts nothing.
        let seq = state.seqs().reserve(&thread.id, 1);
        state.history.append_live(
            &thread.id,
            None,
            seq,
            crate::history_cache::RowSource::Message { at_ms: 1 },
            identity(),
        );
        assert!(matches!(
            state.read_thread_history(&thread.id),
            ThreadHistoryRead::Serve(_)
        ));
        assert!(!state.history.is_loading(&thread.id));

        // The run ends, and the next read asks for the conversation.
        assert!(state.runs.release_thread(&thread.id, &run_id));
        assert!(
            state.start_thread_history_load(&thread.id).is_ok(),
            "a thread with no run is loaded again"
        );
        state.shutdown().unwrap();
    }

    /// A read of an overlay with no run behind it asks for the conversation.
    /// After a restart this is the only thing that turns a diagnostic row into
    /// the thread's actual history, so the read must not be a plain lookup.
    #[tokio::test]
    async fn a_read_of_an_overlay_asks_for_the_conversation() {
        let state = test_state();
        let (thread, _) = state
            .registry
            .create_thread(
                Some(state.registry.personal_project_id()),
                Some("old".into()),
                None,
                loom_relay::now_ms(),
            )
            .unwrap();
        let host_id = HostId::mint();
        state
            .registry
            .set_provider_session_id(
                &thread.id,
                "acp-session-1",
                Some(
                    loom_domain::ProviderSessionBinding::new("pi", "/srv/project").on_host(host_id),
                ),
                loom_relay::now_ms(),
            )
            .unwrap();
        let seq = state.seqs().reserve(&thread.id, 1);
        state.history.append_live(
            &thread.id,
            None,
            seq,
            crate::history_cache::RowSource::Message { at_ms: 1 },
            identity(),
        );
        assert_eq!(
            state.stored_view(&thread.id).unwrap().status,
            HistoryStatus::Partial
        );

        let read = state.read_thread_history(&thread.id);
        assert!(matches!(read, ThreadHistoryRead::Serve(ref view)
            if view.status == HistoryStatus::Partial && view.rows.len() == 1));

        // The read asked for the load. The host here owns no session, so the
        // attempt fails; what it must not do is leave an overlay nobody ever
        // tried to complete. The conversation stays `partial` — that is still
        // an honest description of what is stored — and the *reason* is what
        // changes to the failure.
        let initial = state.stored_view(&thread.id).unwrap().reason;
        for _ in 0..200 {
            if state.stored_view(&thread.id).unwrap().reason != initial {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let view = state.stored_view(&thread.id).unwrap();
        assert_ne!(
            view.reason, initial,
            "the read asked for the conversation: {view:?}"
        );
        assert!(
            view.reason
                .as_deref()
                .is_some_and(|reason| reason.contains("not enrolled")),
            "the stored failure is reported: {view:?}"
        );
        assert_eq!(view.rows.len(), 1, "the overlay stayed visible meanwhile");
        state.shutdown().unwrap();
    }

    #[tokio::test]
    async fn a_failed_load_leaves_no_conversation_behind() {
        let state = test_state();
        let thread = ThreadId::mint();
        let failure = state
            .ensure_history(&thread, cache_binding(), || async {
                Err(HistoryTransportError::Failed {
                    code: "session_missing".to_owned(),
                    message: "the agent no longer has it".to_owned(),
                })
            })
            .await
            .expect_err("a host failure is not a conversation");

        assert_eq!(
            failure,
            HistoryUnavailable::Host {
                code: "session_missing".to_owned(),
                message: "the agent no longer has it".to_owned(),
            }
        );
        let view = state.stored_view(&thread).expect("the failure is recorded");
        assert_eq!(view.status, HistoryStatus::Unavailable);
        assert!(
            view.rows.is_empty(),
            "a partial conversation must never be left claiming to be the conversation"
        );
        state.shutdown().unwrap();
    }

    /// Ten clients opening the same conversation is one load and ten readers.
    #[tokio::test]
    async fn concurrent_callers_share_one_load() {
        let state = test_state();
        let thread = ThreadId::mint();
        let binding = cache_binding();
        let loads = Arc::new(AtomicUsize::new(0));

        let mut callers = Vec::new();
        for _ in 0..4 {
            let state = state.clone();
            let thread = thread.clone();
            let binding = binding.clone();
            let loads = Arc::clone(&loads);
            callers.push(tokio::spawn(async move {
                state
                    .ensure_history(&thread, binding, move || {
                        loads.fetch_add(1, Ordering::SeqCst);
                        async {
                            // Long enough for the other callers to arrive and
                            // become followers.
                            tokio::time::sleep(Duration::from_millis(100)).await;
                            Ok(vec![identity()])
                        }
                    })
                    .await
            }));
        }

        for caller in callers {
            let view = caller
                .await
                .unwrap()
                .expect("every caller gets the conversation");
            assert_eq!(view.rows.len(), 1);
        }
        assert_eq!(loads.load(Ordering::SeqCst), 1, "one load, four readers");
        state.shutdown().unwrap();
    }
}
