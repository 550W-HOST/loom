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

use crate::history_cache::{CacheBinding, CacheView, HistoryStatus, LoadTicket};
use crate::history_rpc::HistoryTransportError;
use crate::state::AppState;

/// One batch of a load, as framed on the wire.
const HISTORY_MAX_BATCH_BYTES: u64 = 256 * 1024;
/// The whole conversation, above which the load fails rather than truncates.
const HISTORY_MAX_TOTAL_BYTES: u64 = 8 * 1024 * 1024;
/// How long a load may take. It covers an agent's cold start and a full replay.
const HISTORY_LOAD_DEADLINE: Duration = Duration::from_secs(90);

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
        let Some(view) = self.history.view(thread_id) else {
            return match self.start_thread_history_load(thread_id) {
                Ok(()) | Err(HistoryUnavailable::Busy) => {
                    ThreadHistoryRead::Loading { reason: None }
                }
                Err(HistoryUnavailable::RunInFlight) => ThreadHistoryRead::Loading {
                    reason: Some(HistoryUnavailable::RunInFlight.to_string()),
                },
                Err(error) => ThreadHistoryRead::Unavailable(error.to_string()),
            };
        };
        match view.status {
            HistoryStatus::Ready | HistoryStatus::Loading | HistoryStatus::Unavailable => {
                ThreadHistoryRead::Serve(view)
            }
            // A stale or partial view is worth showing while a newer one loads.
            // Whether a load may start is the loader's decision, not the
            // reader's: it is the same decision as the claim, and splitting it
            // across two steps is what let a run slip in between them.
            HistoryStatus::Stale | HistoryStatus::Partial => {
                let _ = self.start_thread_history_load(thread_id);
                ThreadHistoryRead::Serve(view)
            }
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
                self.history.mark_loading(thread_id, binding.clone());
                let mark = self.history.append_mark(thread_id);
                let outcome = load().await;
                self.settle_history_load(thread_id, binding, mark, outcome)
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
                self.history.mark_loading(thread_id, binding.clone());
                // What the overlay holds when the load starts. An event that
                // arrives before the replay does means the replay is not the
                // whole conversation any more; see `settle_history_load`.
                let mark = self.history.append_mark(thread_id);
                let state = self.clone();
                let thread_id = thread_id.clone();
                tokio::spawn(async move {
                    let outcome = state
                        .load_thread_history(&binding.host_id, operation, HISTORY_LOAD_DEADLINE)
                        .await;
                    let _ = state.settle_history_load(&thread_id, binding, mark, outcome);
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
    fn settle_history_load(
        &self,
        thread_id: &ThreadId,
        binding: CacheBinding,
        mark: u64,
        outcome: Result<Vec<ProviderEvent>, HistoryTransportError>,
    ) -> Result<CacheView, HistoryUnavailable> {
        self.history.finish_load(thread_id);
        match outcome {
            Ok(events) => {
                // A replay is the whole conversation as of the moment it was
                // collected. If the thread has said something since, this one
                // is not that whole any more, and replacing the overlay with it
                // would drop what just arrived. The result is dropped instead,
                // and the next read after the run ends loads again.
                if !self
                    .history
                    .install_baseline_if_unchanged(thread_id, binding, mark, events)
                {
                    self.history.mark_stale(
                        thread_id,
                        "the thread changed while its history was loading",
                    );
                    self.history_waits.wake(thread_id);
                    return Err(HistoryUnavailable::Incomplete(
                        "the thread changed while its history was loading".to_owned(),
                    ));
                }
            }
            Err(error) => {
                self.history
                    .mark_unavailable(thread_id, binding, error.to_string());
                self.history_waits.wake(thread_id);
                return Err(HistoryUnavailable::from(error));
            }
        }
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

    /// The cached view when it is complete and nothing else is needed.
    fn complete_view(&self, thread_id: &ThreadId) -> Option<CacheView> {
        let view = self.history.view(thread_id)?;
        (view.status == HistoryStatus::Ready).then_some(view)
    }

    /// The cached view, or the reason there is not a usable one.
    fn complete_view_result(&self, thread_id: &ThreadId) -> Result<CacheView, HistoryUnavailable> {
        match self.history.view(thread_id) {
            Some(view) if view.status == HistoryStatus::Ready => Ok(view),
            Some(view) => Err(HistoryUnavailable::Incomplete(
                view.reason
                    .unwrap_or_else(|| "the conversation could not be loaded".to_owned()),
            )),
            None => Err(HistoryUnavailable::NoBinding),
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
    async fn a_complete_conversation_is_not_loaded_again() {
        let state = test_state();
        let thread = ThreadId::mint();
        let binding = cache_binding();
        state
            .history
            .install_baseline(&thread, binding.clone(), vec![identity()]);
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

    /// A refresh is a load: the view is `Stale`, so it is rebuilt under a new
    /// generation rather than served as current.
    #[tokio::test]
    async fn a_stale_conversation_is_rebuilt_under_a_new_generation() {
        let state = test_state();
        let thread = ThreadId::mint();
        let binding = cache_binding();
        let first = state
            .history
            .install_baseline(&thread, binding.clone(), vec![identity()]);
        state.history.mark_stale(&thread, "a turn finished");

        let view = state
            .ensure_history(&thread, binding, || async {
                Ok(vec![identity(), identity(), identity()])
            })
            .await
            .expect("the refresh produced a conversation");

        assert!(view.generation > first, "a rebuild mints a new generation");
        assert_eq!(view.rows.len(), 3);
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
        let view = state.history.view(&thread).unwrap();
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
            state.history.view(&thread.id).unwrap().status,
            HistoryStatus::Partial
        );

        let read = state.read_thread_history(&thread.id);
        assert!(matches!(read, ThreadHistoryRead::Serve(ref view)
            if view.status == HistoryStatus::Partial && view.rows.len() == 1));

        // The read asked for the load. The host here owns no session, so the
        // attempt fails; what it must not do is leave the entry an overlay
        // nobody ever tried to complete.
        for _ in 0..200 {
            if state.history.view(&thread.id).unwrap().status != HistoryStatus::Partial {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let view = state.history.view(&thread.id).unwrap();
        assert_ne!(
            view.status,
            HistoryStatus::Partial,
            "the read asked for the conversation: {view:?}"
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
        let view = state
            .history
            .view(&thread)
            .expect("the failure is recorded");
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
