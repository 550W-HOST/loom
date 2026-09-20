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
            Self::Host { code, message } => {
                write!(f, "the host could not load the conversation ({code}): {message}")
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

impl AppState {
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
                let outcome = load().await;
                self.history.finish_load(thread_id);
                match outcome {
                    Ok(events) => {
                        self.history.install_baseline(thread_id, binding, events);
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
            LoadTicket::Follower => self.await_leader(thread_id).await,
            LoadTicket::Refused => Err(HistoryUnavailable::Busy),
        }
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
    fn complete_view_result(
        &self,
        thread_id: &ThreadId,
    ) -> Result<CacheView, HistoryUnavailable> {
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
        state.shutdown();
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
        state.shutdown();
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
        state.shutdown();
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
        let view = state.history.view(&thread).expect("the failure is recorded");
        assert_eq!(view.status, HistoryStatus::Unavailable);
        assert!(
            view.rows.is_empty(),
            "a partial conversation must never be left claiming to be the conversation"
        );
        state.shutdown();
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
        state.shutdown();
    }
}
