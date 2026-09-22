//! Shared server state.
//!
//! [`AppState`] is deliberately three things and no more: the log, a handle to
//! the fan-out actor, and the readers connecting them. Handlers get this and
//! nothing else — no database handle, no connection registry. A route can
//! publish an event but cannot decide who receives it, which is the property
//! that keeps routing changes out of handlers.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use loom_domain::{DomainEvent, DomainScope, HostId, RunId, RunOutcome, ThreadStatus};
use loom_provider_protocol::ProviderSpec;
use loom_relay::retention::Retention;
use loom_relay::{now_ms, Relay, Result as RelayResult, Scope};
use tokio::sync::broadcast;

use crate::artifacts::Artifacts;
use crate::automations::{self, AutomationsRegistry};
use crate::domain_state::DomainRegistry;
use crate::file_previews::FilePreviewRegistry;
use crate::history_rpc::HistoryBroker;
use crate::host_files::HostFileBroker;
use crate::host_rpc::HostRpcBroker;
use crate::hub_actor::HubHandle;
use crate::join_codes::JoinCodeRegistry;
use crate::persistence::{self, DomainSnapshot, SNAPSHOT_VERSION};
use crate::pump::{PublicRealtimeEvent, Pump, PumpConfig};
use crate::runs::{RunRecord, RunRegistry};
use crate::settings::SettingsRegistry;
use crate::ui::Ui;

/// How many threads' conversations the timeline cache may hold at once.
///
/// The cache is a cache: evicting a thread costs one reload from the host that
/// owns its session, so the bound is about memory rather than correctness.
const HISTORY_CACHE_THREADS: usize = 64;

/// How many bytes of cached conversation rows the server may hold.
const HISTORY_CACHE_BYTES: u64 = 64 * 1024 * 1024;

/// Writes a run's state through to the store as the run changes.
///
/// A run's flags are what a restart reads to settle it: whether its turn
/// started, whether its provider errored, whether its terminal was published,
/// and which status change it still owes. Keeping them only in memory is what
/// made a restart reconstruct them by replaying a bounded log, which is the read
/// this is the first half of replacing.
///
/// A write that fails is reported rather than swallowed: the run continues in
/// memory, but its flags are no longer crash-safe, and saying so is the only
/// honest thing left — the alternative is a restart that settles a run from
/// flags it never had.
struct DurableRuns {
    store: Arc<Mutex<crate::store::Store>>,
}

impl crate::runs::RunSink for DurableRuns {
    fn stored(&self, record: &crate::runs::RunRecord) {
        let store = self
            .store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Err(error) = store.upsert_run(record) {
            eprintln!(
                "loom-server: the state of run {} could not be stored: {error}",
                record.run_id
            );
        }
    }

    fn forgotten(&self, run_id: &loom_domain::RunId) {
        let store = self
            .store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Err(error) = store.forget_run(run_id) {
            eprintln!("loom-server: run {run_id} could not be forgotten: {error}");
        }
    }
}

/// Why a conversation that outlived a stop that did not finish is marked.
///
/// Nothing in the file can say *what* was lost — only that the process that had
/// it did not finish — so this is what a reader is told, and a successful load
/// is what replaces it.
const UNFINISHED_STOP_REASON: &str =
    "the server stopped without finishing; this conversation may be behind";

/// How many conversation rows may wait for the store before a publisher is
/// refused.
///
/// Bounded on purpose: growing without bound would trade a slow disk for the
/// server's memory, and a refusal is reported on the thread rather than hidden.
const STORE_WRITE_QUEUE: usize = 1_024;

/// How many history loads may be in flight at once.
///
/// Each one holds a worker connection and a growing replay, so this is the
/// bound that keeps a burst of cache misses from multiplying memory.
const HISTORY_CACHE_CONCURRENT_LOADS: usize = 4;

/// How the server is wired.
#[derive(Clone, Debug)]
pub struct AppConfig {
    /// This node's identity, stamped on every envelope it produces.
    pub node_id: String,
    /// Approximate records retained per shard.
    pub backend_max_len: usize,
    /// Where the relay log lives on disk.
    ///
    /// `None` keeps the in-process, zero-dependency memory backend, which
    /// is the default so the server starts with no configuration at all.
    /// `Some(path)` uses the durable backend, so the grace window replays
    /// across a restart.
    pub backend_path: Option<PathBuf>,
    /// Retention windows for the log.
    pub retention: Retention,
    /// Bound on the hub actor's command queue.
    pub hub_queue_capacity: usize,
    /// Reader tuning.
    pub pump: PumpConfig,
    /// The id of the host on the *server's* machine, if the operator declared
    /// one.
    ///
    /// Defaults to `None`, which is what makes the server-only path safe: no
    /// local worker is assumed, so primary-host resolution never gets stranded
    /// on an absent local machine. Set `--local-host-id` (or this field) on
    /// a single-machine deployment to prefer that machine while its worker is
    /// attached.
    pub local_host_id: Option<HostId>,
    /// How long a dispatched run may stay *silent* before the server reaps it.
    ///
    /// This is the backstop for a worker that is connected but wedged. The
    /// execution plane enforces its own provider timeout; this one exists so a
    /// silent worker cannot leave a thread `working` forever. It is measured
    /// from the run's last report, not from its dispatch, and a run with an
    /// item reported started and not completed holds it off — see
    /// [`crate::runs::RunRecord::refresh_deadline`].
    pub run_timeout: Duration,
    /// How long a dispatched run may take *in total* before the server reaps
    /// it, however active it is.
    ///
    /// The last-resort bound, matching the worker's own ceiling: only a worker
    /// wedged with a tool call still open reaches it, because a still-reporting
    /// run keeps pushing the silence deadline out. Set it above
    /// [`AppConfig::run_timeout`]; `Duration::ZERO` removes it.
    pub run_ceiling: Duration,
    /// How long a host may go without a heartbeat before it is considered gone
    /// and its in-flight runs are failed.
    ///
    /// Must comfortably exceed the worker's heartbeat interval; the default is
    /// four times the worker default.
    pub host_stale_after: Duration,
    /// How often the server runs the timeout/staleness sweep. `Duration::ZERO`
    /// disables the background sweep, which is what unit tests want when they
    /// drive reconciliation explicitly.
    pub reconcile_interval: Duration,
    /// How often the automation scheduler looks for due windows.
    ///
    /// `Duration::ZERO` disables it, which is what a test wants when it drives
    /// the sweep itself. The default matches the reference sweep's cadence.
    pub schedule_interval: Duration,
    /// How often the entity view is written to the store.
    ///
    /// Only meaningful when [`AppConfig::backend_path`] names a data
    /// directory: the in-process default keeps no domain state, so it has no
    /// local snapshot to write.
    /// `Duration::ZERO` disables the periodic writer; a snapshot is still
    /// written once, on a clean shutdown.
    pub entity_write_interval: Duration,
    /// The agents the control plane can ask execution machines to run, in
    /// preference order.
    ///
    /// One entry is the normal case today (`pi`), but the list is what makes a
    /// second ACP agent — `codex`, `claude-code`, `omp` — a configuration
    /// change rather than a rewrite: a provider is identified by
    /// [`ProviderSpec::name`] everywhere on the wire, and the first entry is
    /// the default a caller that has not chosen one gets.
    ///
    /// An empty list is normalised to `[pi]` when the state is built, so the
    /// accessors can promise at least one provider.
    pub providers: Vec<ProviderSpec>,
    /// A frontend dev server to reverse-proxy unmatched requests to.
    ///
    /// Development only: the product app is embedded in the binary, and this is
    /// the one override that lets a dev server serve the client instead while
    /// `/api`, `/ws` and `/internal/ws` stay here.
    pub ui_proxy: Option<String>,
    /// Where worker binaries are hosted for self-update.
    ///
    /// `None` falls back to the directory holding the running `loom-server`,
    /// which is where `install.sh` puts the matching `loom-worker`. Set it when
    /// the two binaries are not side by side (a container, or a server that
    /// hosts another machine's artifacts).
    pub artifact_dir: Option<PathBuf>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            node_id: "loom-node".into(),
            backend_max_len: 2_000,
            backend_path: None,
            retention: Retention::default(),
            hub_queue_capacity: 1_024,
            pump: PumpConfig::default(),
            local_host_id: None,
            run_timeout: Duration::from_secs(30 * 60),
            // Six hours: far above any real turn, so it only decides the fate of
            // a worker that wedged with a tool call open. It must stay at least
            // the worker's own ceiling, or the server would reap the runs the
            // worker is still willing to nurse.
            run_ceiling: Duration::from_secs(6 * 60 * 60),
            host_stale_after: Duration::from_secs(60),
            reconcile_interval: Duration::from_secs(5),
            schedule_interval: Duration::from_secs(10),
            entity_write_interval: Duration::from_secs(30),
            providers: vec![ProviderSpec::pi()],
            ui_proxy: None,
            artifact_dir: None,
        }
    }
}

/// Raised when the server cannot be wired up.
#[derive(Debug)]
pub struct BuildStateError {
    message: String,
}

impl std::fmt::Display for BuildStateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for BuildStateError {}

/// What a shutdown could not finish.
///
/// Only the log flush is fatal enough to return: everything else a shutdown
/// does is either already durable or derived. A process that was asked to stop
/// can still fail its exit code when its last write did not reach the disk.
#[derive(Debug)]
pub struct ShutdownError {
    message: String,
}

impl std::fmt::Display for ShutdownError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ShutdownError {}

impl From<loom_relay::RelayError> for BuildStateError {
    fn from(error: loom_relay::RelayError) -> Self {
        Self {
            message: error.to_string(),
        }
    }
}

impl From<crate::store::StoreError> for BuildStateError {
    fn from(error: crate::store::StoreError) -> Self {
        Self {
            message: format!("the store could not be opened: {error}"),
        }
    }
}

/// The agents each host reported, in enrollment order.
type HostProviders = Arc<Mutex<Vec<(HostId, Vec<ProviderSpec>)>>>;

/// Everything a handler needs.
#[derive(Clone)]
pub struct AppState {
    /// The relay log.
    pub relay: Relay,
    /// Handle to the per-node fan-out actor.
    pub hub: HubHandle,
    /// The fixed shard readers.
    pub pump: Arc<Pump>,
    /// Complete relay stream projected into the typed public realtime socket.
    pub(crate) public_events: broadcast::Sender<PublicRealtimeEvent>,
    /// In-memory domain entities for the command API.
    pub registry: Arc<DomainRegistry>,
    /// Provider runs that have been dispatched and not yet terminated.
    pub runs: Arc<RunRegistry>,
    /// Server-local settings and UI preferences.
    pub settings: Arc<SettingsRegistry>,
    /// Automations and their run history.
    pub automations: Arc<AutomationsRegistry>,
    /// The static UI source the fallback route serves.
    pub ui: Ui,
    /// The worker binaries this server hosts for self-update.
    pub artifacts: Arc<Artifacts>,
    /// One-time host enrollment capabilities.
    pub join_codes: Arc<JoinCodeRegistry>,
    /// Short-lived root-bound capabilities for host file previews.
    pub file_previews: Arc<FilePreviewRegistry>,
    /// HTTP requests waiting on a host's answer to a file read or listing.
    pub host_files: Arc<HostFileBroker>,
    /// HTTP requests waiting on a host's answer to a workspace RPC.
    pub host_rpc: Arc<HostRpcBroker>,
    /// Callers waiting on a host's streamed history load.
    pub history_rpc: Arc<HistoryBroker>,
    /// The per-thread timeline cache: a rebuildable, bounded view of a
    /// conversation whose authority lives with the agent that owns it.
    /// The server's persistent store, opened with the server.
    ///
    /// A server with a data directory gets a file in it; a temporary server
    /// (tests, a throwaway process) gets an in-memory store with the same
    /// schema. One code path serves both on purpose: there is no branch where a
    /// server runs with persistence quietly switched off, only one where the
    /// store has nowhere to live.
    store: Arc<Mutex<crate::store::Store>>,
    /// The thread that turns conversation rows into transactions.
    store_writer: Arc<crate::store::StoreWriter>,
    /// The numbering every conversation row is written in, seeded from the
    /// store so a restart continues a conversation instead of numbering over it.
    seqs: Arc<crate::store::SeqAllocator>,
    pub history: Arc<crate::history_cache::HistoryCache>,
    /// Who is waiting on which in-flight history load.
    pub history_waits: Arc<crate::history::HistoryWaits>,
    /// HTTP requests waiting on a host's answer to a terminal operation.
    pub terminal: Arc<crate::terminals::TerminalBroker>,
    /// What each host's agent reported it can run.
    ///
    /// Kept per host because it is a fact about the agent on that machine, not
    /// about the control plane's own configuration. See [`crate::catalogs`].
    pub catalogs: Arc<crate::catalogs::CatalogRegistry>,
    /// The prompt commands each workspace's most recent ACP session
    /// advertised. See [`crate::commands`].
    pub commands: Arc<crate::commands::CommandRegistry>,
    /// The control plane's terminal session index.
    pub terminals: Arc<crate::terminals::TerminalSessions>,
    local_host_id: Option<HostId>,
    run_timeout_ms: u64,
    run_ceiling_ms: u64,
    host_stale_after_ms: u64,
    /// The agents the operator listed for the control plane itself.
    ///
    /// A fallback, not the source of truth: what each host discovered is.
    provider_specs: Vec<ProviderSpec>,
    /// The agents each host found installed, in enrollment order.
    ///
    /// A `Vec` rather than a map so the advertised order is stable: the first
    /// provider is the default, and a set that reordered itself between
    /// requests would change which agent a thread with no explicit choice runs
    /// on. Re-enrollment replaces a host's entry in place.
    host_providers: HostProviders,
    reconcile_stop: Arc<AtomicBool>,
    entity_write_stop: Arc<AtomicBool>,
    schedule_stop: Arc<AtomicBool>,
    entity_write_lock: Arc<Mutex<()>>,
    started_at: Instant,
    started_at_ms: u64,
}

/// Appends `spec` unless an Agent of the same name is already listed.
///
/// Merging hosts and configuration by name keeps one entry per agent while
/// preserving the order the entries were first seen in.
fn push_once(specs: &mut Vec<ProviderSpec>, spec: &ProviderSpec) {
    if !specs.iter().any(|known| known.name == spec.name) {
        specs.push(spec.clone());
    }
}

impl AppState {
    /// The server's store.
    ///
    /// A short lock, held for one operation: the store is one SQLite connection
    /// and SQLite serializes writes itself, so the mutex is only there because a
    /// connection is not `Sync`.
    pub fn store_writer(&self) -> &crate::store::StoreWriter {
        &self.store_writer
    }

    pub fn seqs(&self) -> &crate::store::SeqAllocator {
        &self.seqs
    }

    pub fn store(&self) -> std::sync::MutexGuard<'_, crate::store::Store> {
        self.store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Wires the relay, hub actor and readers together.
    ///
    /// The relay's frames live in the store: the same database that holds the
    /// conversations and the entity view is the whole durable state, and the
    /// shard files this used to write are gone. A store is therefore opened
    /// here — before the relay — and handed to both.
    pub fn build(config: AppConfig) -> Result<Self, BuildStateError> {
        let store = match &config.backend_path {
            Some(path) => crate::store::Store::open(path.join("loom.db"))?,
            None => crate::store::Store::open_in_memory()?,
        };
        let store = Arc::new(Mutex::new(store));
        let backend: loom_relay::SharedBackend = Arc::new(crate::store::StoreBackend::new(
            Arc::clone(&store),
            config.backend_max_len,
        ));
        Self::build_with_store(config, backend, store)
    }

    #[cfg(test)]
    pub(crate) fn build_for_test(
        config: AppConfig,
        backend: loom_relay::SharedBackend,
    ) -> Result<Self, BuildStateError> {
        Self::build_from_backend(config, backend)
    }

    /// Wires a state over an injected backend, for a test that wants to choose
    /// the storage the relay runs on.
    #[cfg(test)]
    fn build_from_backend(
        config: AppConfig,
        backend: loom_relay::SharedBackend,
    ) -> Result<Self, BuildStateError> {
        let store = match &config.backend_path {
            Some(path) => crate::store::Store::open(path.join("loom.db"))?,
            None => crate::store::Store::open_in_memory()?,
        };
        let store = Arc::new(Mutex::new(store));
        Self::build_with_store(config, backend, store)
    }

    /// Wires a state over a store and a relay backend that both already exist.
    fn build_with_store(
        config: AppConfig,
        backend: loom_relay::SharedBackend,
        store: Arc<Mutex<crate::store::Store>>,
    ) -> Result<Self, BuildStateError> {
        // A store the last process was killed in the middle of may be missing
        // the tail a published-and-unwritten conversation held. What is stored
        // is kept and marked as possibly behind — not thrown away, and not
        // called complete — and the ordinary read path reconciles it by asking
        // the agent for the conversation again.
        if store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .recovered_from_unclean_stop()
        {
            let marked = store
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .mark_stored_history_behind(UNFINISHED_STOP_REASON);
            match marked {
                Ok(0) => {}
                Ok(count) => eprintln!(
                    "loom-server: the store was left by a stop that did not finish; \
                     {count} stored conversation(s) are marked as possibly behind"
                ),
                Err(error) => eprintln!(
                    "loom-server: marking stored conversations behind after an \
                     unfinished stop failed: {error}"
                ),
            }
        }
        let seqs = Arc::new(crate::store::SeqAllocator::seeded_from(
            &store
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        )?);
        // The overlay has to exist before the writer, because the writer tells
        // it which rows are committed.
        let history = Arc::new(crate::history_cache::HistoryCache::new(
            HISTORY_CACHE_THREADS,
            HISTORY_CACHE_BYTES,
            HISTORY_CACHE_CONCURRENT_LOADS,
        ));
        let store_writer = Arc::new(crate::store::StoreWriter::spawn(
            Arc::clone(&store),
            STORE_WRITE_QUEUE,
            Arc::clone(&history) as Arc<dyn crate::store::WrittenRows>,
        ));
        // Runs are durable as they change, not only at the periodic write: a
        // restart settles them from these records.
        let runs = Arc::new(RunRegistry::new());
        runs.set_sink(Arc::new(DurableRuns {
            store: Arc::clone(&store),
        }));
        let ui = Ui::from_config(config.ui_proxy.clone())
            .map_err(|message| BuildStateError { message })?;
        let artifacts = Arc::new(Artifacts::from_config(config.artifact_dir.clone()));
        let relay = Relay::new(backend, config.retention, config.node_id.clone())?;
        let (hub, _actor) = HubHandle::spawn(config.hub_queue_capacity);
        let (public_events, _) = broadcast::channel(config.hub_queue_capacity.max(1));
        let pump = Arc::new(Pump::spawn_with_public_events(
            relay.clone(),
            hub.clone(),
            public_events.clone(),
            config.pump,
        ));
        let started_at_ms = now_ms();

        // The entity view is durable on a store that outlives the process; the
        // data directory holds one database: the conversations, the entity
        // view and the relay's frames (see `docs/domain-persistence.md`).
        // Normalised once, so every accessor can promise at least one provider.
        let provider_specs = if config.providers.is_empty() {
            vec![ProviderSpec::pi()]
        } else {
            config.providers
        };
        let provider_id = provider_specs
            .first()
            .expect("the provider list is normalised to at least one entry")
            .name
            .clone();

        let state = Self {
            relay,
            hub,
            pump,
            public_events,
            registry: Arc::new(DomainRegistry::new(started_at_ms)),
            runs,
            settings: Arc::new(SettingsRegistry::new(&provider_id)),
            automations: Arc::new(AutomationsRegistry::new()),
            ui,
            artifacts,
            file_previews: Arc::new(FilePreviewRegistry::new()),
            join_codes: Arc::new(JoinCodeRegistry::new()),
            host_files: Arc::new(HostFileBroker::new()),
            host_rpc: Arc::new(HostRpcBroker::new()),
            history_rpc: Arc::new(HistoryBroker::new()),
            history,
            store,
            store_writer,
            seqs,
            history_waits: Arc::new(crate::history::HistoryWaits::new()),
            terminal: Arc::new(crate::terminals::TerminalBroker::new()),
            catalogs: Arc::new(crate::catalogs::CatalogRegistry::new()),
            commands: Arc::new(crate::commands::CommandRegistry::new()),
            terminals: Arc::new(crate::terminals::TerminalSessions::new()),
            local_host_id: config.local_host_id,
            run_timeout_ms: config.run_timeout.as_millis().min(u128::from(u64::MAX)) as u64,
            run_ceiling_ms: config.run_ceiling.as_millis().min(u128::from(u64::MAX)) as u64,
            host_stale_after_ms: config
                .host_stale_after
                .as_millis()
                .min(u128::from(u64::MAX)) as u64,
            provider_specs,
            host_providers: Arc::new(Mutex::new(Vec::new())),
            reconcile_stop: Arc::new(AtomicBool::new(false)),
            entity_write_stop: Arc::new(AtomicBool::new(false)),
            schedule_stop: Arc::new(AtomicBool::new(false)),
            entity_write_lock: Arc::new(Mutex::new(())),
            started_at: Instant::now(),
            started_at_ms,
        };

        // Restore the entity view *before* any run is reconciled: the restored
        // threads are what reconciliation and dispatch act on.
        state.recover();

        // The reconciler is the guarantee that a run reaches a terminal state
        // when the execution plane can no longer speak for it. Tests disable
        // it and call `reconcile_runs` directly.
        if !config.reconcile_interval.is_zero() {
            state.spawn_reconciler(config.reconcile_interval);
        }
        if !config.entity_write_interval.is_zero() {
            state.spawn_entity_writer(config.entity_write_interval);
        }
        // The scheduler is what actually fires automations. Tests disable it and
        // call `sweep_automations` themselves.
        if !config.schedule_interval.is_zero() {
            state.spawn_scheduler(config.schedule_interval);
        }

        Ok(state)
    }

    /// Starts the timeout/staleness sweep.
    fn spawn_reconciler(&self, interval: Duration) {
        let state = self.clone();
        let stop = Arc::clone(&self.reconcile_stop);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // The first tick fires immediately; there is nothing to reconcile
            // on a freshly built server.
            ticker.tick().await;
            loop {
                ticker.tick().await;
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                state.reconcile_runs(now_ms());
                // A script runs on a machine, so it is reaped by the same tick
                // that reaps provider runs: a host that went away cannot report
                // one, and a run left in flight holds its single-flight slot.
                state.reconcile_script_runs(now_ms());
            }
        });
    }

    /// Starts the automation scheduler.
    fn spawn_scheduler(&self, interval: Duration) {
        let state = self.clone();
        let stop = Arc::clone(&self.schedule_stop);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // The first tick fires immediately, and it should: unlike the
            // reconciler this one has real work to do on a freshly built
            // server, namely a window that arrived while the process was down.
            loop {
                ticker.tick().await;
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                state.sweep_automations(now_ms());
            }
        });
    }

    /// One automation pass: claim the windows that are due, then dispatch the
    /// runs that are queued, persisting when either changed anything.
    ///
    /// The two halves are ordered because they are the pipeline: the sweep
    /// decides *when* a run is owed, the executor decides *where* it runs. A
    /// manual `run` request calls the executor half directly, so a client does
    /// not wait for the next tick.
    ///
    /// The write is synchronous and conditional for the same reason a settings
    /// write is: the claim that queues a run must be on disk before the run can
    /// be observed, or a restart would find the window due again and fire it
    /// twice.
    pub fn sweep_automations(&self, now_ms: u64) -> automations::SweepReport {
        let report = self.automations.sweep_due(now_ms);
        let executed = self.execute_pending_automation_runs(now_ms);
        if report.changed() || executed.changed() {
            if let Err(error) = self.write_entity_view() {
                eprintln!("loom-server: persisting scheduled automation runs failed: {error}");
            }
        }
        // After the write, never before: a client told to refetch must find the
        // run that made it do so. A manual run publishes its own frame where the
        // client asked for it; a scheduled one is announced here, because this
        // is the only half of the pipeline that knows whose schedules fired.
        for project_id in &report.claimed_projects {
            self.publish_automations_changed(&project_id.to_string());
        }
        if let Some(diagnostic) = report.diagnostic() {
            eprintln!("loom-server: {diagnostic}");
        }
        report
    }

    /// Starts the periodic entity-view writer.
    fn spawn_entity_writer(&self, interval: Duration) {
        let state = self.clone();
        let stop = Arc::clone(&self.entity_write_stop);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // The first tick fires immediately; `build` has just recovered or
            // written nothing, so there is no point snapshotting at once.
            ticker.tick().await;
            loop {
                ticker.tick().await;
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                if let Err(error) = state.write_entity_view() {
                    eprintln!("loom-server: the periodic entity-view write failed: {error}");
                }
            }
        });
    }

    /// How long a run may stay silent before it is reaped.
    pub(crate) fn run_timeout_ms(&self) -> u64 {
        self.run_timeout_ms
    }

    /// How long a run may take in total before it is reaped.
    pub(crate) fn run_ceiling_ms(&self) -> u64 {
        self.run_ceiling_ms
    }

    /// How long a host may go quiet before its runs are reaped.
    pub(crate) fn host_stale_after_ms(&self) -> u64 {
        self.host_stale_after_ms
    }

    /// Records the agents a host found installed, as it reported them at
    /// enrollment.
    ///
    /// The host's list replaces whatever it reported before, so a machine that
    /// uninstalled an agent stops offering it on its next connection. Nothing is
    /// merged across hosts: a provider is only offered because some machine
    /// found it, and a lost report is the honest signal that it is gone.
    pub fn record_host_providers(&self, host_id: &HostId, providers: Vec<ProviderSpec>) {
        let mut hosts = self
            .host_providers
            .lock()
            .expect("the host provider lock is never poisoned");
        match hosts.iter_mut().find(|(id, _)| id == host_id) {
            Some((_, reported)) => *reported = providers,
            None => hosts.push((host_id.clone(), providers)),
        }
    }

    /// Every agent the control plane can dispatch, in preference order.
    ///
    /// The operator's list comes first because it decides the default: the
    /// first entry is the agent a thread that chose none runs on, and a machine
    /// finding extra agents must not silently change that. Discovered agents
    /// follow, so the client can offer them without the control plane having had
    /// to know about them in advance.
    ///
    /// A server with no connected worker therefore answers with exactly what it
    /// did before anything was discovered.
    pub fn providers(&self) -> Vec<ProviderSpec> {
        let mut specs: Vec<ProviderSpec> = Vec::new();
        for spec in &self.provider_specs {
            push_once(&mut specs, spec);
        }
        {
            let hosts = self
                .host_providers
                .lock()
                .expect("the host provider lock is never poisoned");
            for (_, reported) in hosts.iter() {
                for spec in reported {
                    push_once(&mut specs, spec);
                }
            }
        }
        // The configured list is normalised at construction, so this is only
        // empty if a caller somehow built state without one.
        if specs.is_empty() {
            specs.push(ProviderSpec::pi());
        }
        specs
    }

    /// Forgets the agents a host reported, because it is no longer connected.
    ///
    /// A disconnected machine is not evidence of anything. What it last sent may
    /// be a partial list — an incremental report whose remaining probes died
    /// with the worker — and an entry nobody is still confirming must not stay on
    /// offer. A reconnecting host reports from scratch, so nothing is lost by
    /// dropping it.
    pub fn forget_host_providers(&self, host_id: &HostId) {
        let mut hosts = self
            .host_providers
            .lock()
            .expect("the host provider lock is never poisoned");
        hosts.retain(|(id, _)| id != host_id);
    }

    /// The agent a caller gets when it has not chosen one.
    ///
    /// This is the first configured provider — discovery appends to the list, it
    /// never displaces the operator's default. Callers that resolve a provider
    /// from a request must use [`AppState::provider_spec_by_id`] instead, so a
    /// request for one agent's models is never answered with another's.
    ///
    /// `pub` because a route's response (`projects.commands`) names the
    /// provider it runs, and an integration test exercises that route.
    pub fn provider_spec(&self) -> ProviderSpec {
        self.providers()
            .into_iter()
            .next()
            .expect("the provider list always has at least one entry")
    }

    /// The offered agent with this id, when a host reported one or the operator
    /// listed one.
    pub fn provider_spec_by_id(&self, provider_id: &str) -> Option<ProviderSpec> {
        self.providers()
            .into_iter()
            .find(|spec| spec.name == provider_id)
    }

    /// The agent with this id **on this host**, for a dispatch.
    ///
    /// Two machines can report the same agent name with different executables,
    /// and a dispatch must carry the one belonging to the machine that will run
    /// it. The host's own report is therefore consulted first; the operator's
    /// list is the fallback for a server that declared an agent no host
    /// reported, and `None` lets the caller fall back to the default exactly as
    /// it does for an id nothing knows.
    pub fn provider_spec_for_host(
        &self,
        host_id: &HostId,
        provider_id: &str,
    ) -> Option<ProviderSpec> {
        {
            let hosts = self
                .host_providers
                .lock()
                .expect("the host provider lock is never poisoned");
            if let Some((_, reported)) = hosts.iter().find(|(id, _)| id == host_id) {
                if let Some(spec) = reported.iter().find(|spec| spec.name == provider_id) {
                    return Some(spec.clone());
                }
            }
        }
        self.provider_specs
            .iter()
            .find(|spec| spec.name == provider_id)
            .cloned()
    }

    /// The operator-declared local host, if any.
    ///
    /// `None` is the normal server-only state: this server does not claim any
    /// machine as local, so primary-host queries go straight to enrolled
    /// hosts instead of a local fallback.
    pub fn local_host_id(&self) -> Option<&HostId> {
        self.local_host_id.as_ref()
    }

    /// Publishes a frame and wakes the readers.
    ///
    /// This is the whole producer API. Note what it does not do: it never
    /// consults a subscriber, a socket or a room.
    ///
    /// The bytes stored in the log are the **client-facing frame**
    /// (`{"type":"event",...}`), built here before the store so a replayed
    /// frame is identical to the live one. See [`crate::protocol`].
    pub fn publish(
        &self,
        scope: loom_relay::Scope,
        payload: impl Into<bytes::Bytes>,
    ) -> RelayResult<loom_relay::Envelope> {
        let payload = payload.into();
        let frame_scope = scope.clone();
        let envelope = match self
            .relay
            .publish_with(scope, move |event_id, created_at_ms| {
                crate::protocol::build_event_frame(&frame_scope, &payload, event_id, created_at_ms)
            }) {
            Ok(envelope) => envelope,
            Err(error) => {
                let _ = self.public_events.send(PublicRealtimeEvent::Reset);
                return Err(error);
            }
        };
        self.pump.wake();
        Ok(envelope)
    }

    /// Publishes a domain event to the scope the domain assigned it.
    ///
    /// This is the only place the server translates a
    /// [`loom_domain::DomainScope`] into a [`loom_relay::Scope`]; handlers deal
    /// only in domain events. The stored payload is the serialized event, so a
    /// client dispatches on its `type` tag and replay is byte-identical.
    pub fn publish_domain_event(
        &self,
        event: &loom_domain::DomainEvent,
    ) -> RelayResult<loom_relay::Envelope> {
        // The live overlay is fed here, at the seam where an event is applied,
        // rather than from the relay readers: a reader advances behind the log
        // and can be overtaken by a trim, and an overlay that misses events is
        // a conversation with holes in the middle of it.
        self.cache_live_event(event);
        let payload = serde_json::to_vec(event).expect("a DomainEvent always serializes to JSON");
        self.publish(relay_scope(&event.scope()), payload)
    }

    /// Adds one live thread event to the timeline cache's overlay.
    ///
    /// Three shapes reach the timeline: a run's provider frames, and the
    /// messages the control plane appends itself — the user's prompt, and a
    /// reply posted through the messages API. All go in, because a thread whose
    /// agent has not reported a session yet still has a conversation as far as
    /// the user is concerned — the messages they just sent and received.
    ///
    /// The frame carries where it came from, because a projection must not have
    /// to guess: a run's frame belongs to that run and knows when it happened,
    /// and a message the control plane appended is a whole message rather than a
    /// step of one. A message is converted to the same provider frame an agent
    /// would have replayed for that role (`ItemStarted` for a prompt,
    /// `ItemCompleted` for an answer), so the live and restored projections
    /// build one row from one frame.
    ///
    /// The binding is offered when it is known and omitted when it is not: a
    /// thread with no session yet has events but nothing to load a history
    /// from, which the cache records as a partial conversation rather than
    /// pretending it is the whole one.
    fn cache_live_event(&self, event: &loom_domain::DomainEvent) {
        use loom_domain::{MessageRole, ProviderEvent};
        let (thread_id, source, body) = match event {
            loom_domain::DomainEvent::ThreadRunEvent { run } => (
                run.thread_id.clone(),
                crate::history_cache::RowSource::Run {
                    run_id: run.run_id.clone(),
                    at_ms: run.at_ms,
                },
                run.event.body.clone(),
            ),
            loom_domain::DomainEvent::ThreadMessageAdded { thread_id, message } => {
                let frame = match message.role {
                    MessageRole::User => ProviderEvent::ItemStarted {
                        item: loom_domain::ThreadEventItem::UserMessage {
                            id: message.id.to_string(),
                            content: vec![loom_domain::UserContent::Text {
                                text: message.content.clone(),
                            }],
                            client_request_id: None,
                            parent_tool_call_id: None,
                        },
                        provider_thread_id: String::new(),
                    },
                    MessageRole::Assistant => ProviderEvent::ItemCompleted {
                        item: loom_domain::ThreadEventItem::AgentMessage {
                            id: message.id.to_string(),
                            text: message.content.clone(),
                            presentation: None,
                            parent_tool_call_id: None,
                        },
                        provider_thread_id: String::new(),
                    },
                    // A system message has no provider frame to be: the
                    // messages API does not accept the role, so nothing
                    // produces one, and inventing a frame for it would put a
                    // row on the timeline that no agent ever said.
                    MessageRole::System => return,
                };
                (
                    thread_id.clone(),
                    crate::history_cache::RowSource::Message {
                        at_ms: message.created_at_ms,
                    },
                    frame,
                )
            }
            _ => return,
        };

        let Some(thread) = self.registry.thread(&thread_id) else {
            return;
        };
        let binding = match (
            thread.provider_session_id.clone(),
            thread.provider_session_binding.as_ref(),
        ) {
            (Some(provider_session_id), Some(binding)) => {
                binding
                    .host_id
                    .clone()
                    .map(|host_id| crate::history_cache::CacheBinding {
                        host_id,
                        agent: binding.agent.clone(),
                        provider_session_id,
                        cwd: binding.cwd.clone(),
                    })
            }
            _ => None,
        };
        // The number is reserved once and travels with the row into both
        // places it exists: the overlay a reader sees now, and the store the row
        // is written to behind it. Reserving it here — rather than letting each
        // side count its own — is what makes the two agree, and what lets a
        // restarted server continue the conversation instead of numbering over
        // it.
        let seq = self.seqs.reserve(&thread_id, 1);
        // Display first: the overlay is what a reader sees, and it must not wait
        // for a disk. The store gets the same row behind it, through a bounded
        // queue — a refusal or a failed write marks the thread as unsaved rather
        // than growing without bound or pretending it was stored.
        self.history.append_live(
            &thread_id,
            binding.as_ref(),
            seq,
            source.clone(),
            body.clone(),
        );
        self.store_writer.enqueue(&thread_id, seq, source, body);
    }

    /// Publishes a durable public cache invalidation with no domain-event peer.
    ///
    /// Server-local settings do not belong in the domain aggregate, but their
    /// browser caches still need a replayable invalidation shared by all nodes.
    pub(crate) fn publish_public_change(
        &self,
        message: &crate::protocol::ServerMessage,
    ) -> RelayResult<loom_relay::Envelope> {
        let payload =
            serde_json::to_vec(message).expect("a public realtime message always serializes");
        self.publish(Scope::Global, payload)
    }

    /// Milliseconds since the server was wired up.
    pub fn uptime_ms(&self) -> u64 {
        self.started_at.elapsed().as_millis() as u64
    }

    /// Wall-clock milliseconds at startup.
    pub fn started_at_ms(&self) -> u64 {
        self.started_at_ms
    }

    /// Restores the entity view from a durable snapshot plus the log delta.
    ///
    /// This runs once, from [`AppState::build`], before anything can observe
    /// the state. It never fails: a snapshot that cannot be trusted is treated
    /// as absent and the log is replayed from the beginning, because refusing
    /// to start is a worse outcome than starting from a consistent older view.
    fn recover(&self) {
        let now = now_ms();
        // The store is the entity view's home: it is the only place a view is
        // read from, and the only place one is written to. A store that cannot
        // be read is not a reason to refuse to start — the log is still there to
        // rebuild from, which is what it is for.
        let snapshot = match self.store().entities() {
            Ok(snapshot) => snapshot,
            Err(error) => {
                eprintln!(
                    "loom-server: the stored entity view could not be read ({error}); \
                     rebuilding from the log"
                );
                None
            }
        };
        match snapshot {
            Some(snapshot) => {
                let watermark = snapshot.watermark;
                let runs = snapshot.runs.clone();
                self.registry.restore(snapshot.registry);
                if let Some(settings) = snapshot.settings {
                    self.settings.restore(settings, &self.provider_spec().name);
                }
                if let Some(automations) = snapshot.automations {
                    self.automations.restore(automations);
                }
                let interrupted = self.automations.fail_interrupted_runs(now);
                if !interrupted.is_empty() {
                    eprintln!(
                        "loom-server: failed {} automation run(s) that were in flight when the \
                         server stopped",
                        interrupted.len()
                    );
                }
                let replayed = self.replay_domain_events(watermark);
                let failed = self.fail_in_flight_runs(runs, now);
                eprintln!(
                    "loom-server: restored the entity view ({replayed} log events replayed, \
                     {failed} in-flight runs failed)"
                );
            }
            None => {
                // No stored view and no readable file: the retained log is the
                // only thing to go on.
                let replayed = self.replay_domain_events(None);
                let failed = self.fail_in_flight_runs(Vec::new(), now);
                if replayed > 0 || failed > 0 {
                    eprintln!(
                        "loom-server: no stored entity view; rebuilt {replayed} events from the \
                         log ({failed} in-flight runs failed)"
                    );
                }
            }
        }
    }

    /// Applies every retained domain event newer than `after` to the registry.
    ///
    /// Replay is applied per shard, in append order, which is enough: all of an
    /// entity's events share its scope's shard, so per-entity order is
    /// preserved without a global merge. Cross-entity order never matters to
    /// the entity view. Returns how many events were applied.
    fn replay_domain_events(&self, after: Option<loom_relay::EventId>) -> usize {
        let mut applied = 0;
        for shard in 0..loom_relay::SHARD_COUNT {
            let records = match self.relay.read_shard_after(shard, None, usize::MAX) {
                Ok(records) => records,
                Err(error) => {
                    eprintln!("loom-server: replaying shard {shard} failed: {error}");
                    continue;
                }
            };
            // The log drops its oldest records under the per-shard cap, so a
            // cursor can fall off the start of the window. Detect it and say so
            // rather than pretend the delta was complete; the snapshot still
            // covers everything up to the cursor, so the result is stale, never
            // wrong.
            if let (Some(cursor), Some(first)) = (after, records.first()) {
                if first.event_id > cursor {
                    eprintln!(
                        "loom-server: shard {shard} no longer holds events after the snapshot \
                         watermark; the gap cannot be replayed"
                    );
                }
            }
            for envelope in records {
                if after.is_some_and(|cursor| envelope.event_id <= cursor) {
                    continue;
                }
                if let Some(event) = domain_event_from_envelope(&envelope) {
                    self.registry.apply_event(&event);
                    applied += 1;
                }
            }
        }
        applied
    }

    /// Settles every run a restart did not survive.
    ///
    /// A stored record is the authority on how its run ended: its verdict is
    /// written before the frame it is about, so a crash leaves a run whose
    /// ending is known and whose frame may be missing. Nothing here reads the
    /// log to find out what a run did — that is the read this replaces.
    fn fail_in_flight_runs(&self, records: Vec<RunRecord>, now: u64) -> usize {
        let mut restored = Vec::new();
        for record in records {
            let verdict = record.terminal_outcome;
            let Some(thread) = self.registry.thread(&record.thread_id) else {
                continue;
            };
            if verdict.is_none()
                && !matches!(thread.status, ThreadStatus::Working | ThreadStatus::Waiting)
            {
                continue;
            }
            restored.push((record, verdict));
        }
        self.runs
            .restore(restored.iter().map(|(record, _)| record.clone()));

        let mut failed = 0;
        for (record, verdict) in &restored {
            let settled = match verdict {
                Some(outcome) => self.settle_run_from_verdict(record, *outcome, now),
                None => self.fail_run_after_restart(record, now),
            };
            if settled {
                failed += 1;
            }
        }
        // A thread that is still `working` with no stored run is a thread whose
        // run the store never saw — a dispatch the process did not survive. It
        // has no verdict to trust, so it is failed from what the thread itself
        // recorded, and a run id it never got is minted for the terminal.
        for thread in self.registry.threads() {
            if !matches!(thread.status, ThreadStatus::Working | ThreadStatus::Waiting) {
                continue;
            }
            let (record, verdict) = self.runs.for_thread(&thread.id).map_or_else(
                || {
                    let record = RunRecord {
                        run_id: thread.active_run_id.clone().unwrap_or_else(RunId::mint),
                        thread_id: thread.id.clone(),
                        project_id: thread.project_id.clone(),
                        host_id: HostId::mint(),
                        cwd: String::new(),
                        started_at_ms: now,
                        deadline_ms: now,
                        last_event_ms: None,
                        open_items: 0,
                        turn_started: false,
                        provider_thread_id: None,
                        provider_id: None,
                        provider_error_reported: false,
                        failure_reason: None,
                        terminal_published: false,
                        terminal_outcome: None,
                        pending_status_event: None,
                    };
                    self.runs.insert(record.clone());
                    (record, None)
                },
                |record| {
                    let verdict = record.terminal_outcome;
                    (record, verdict)
                },
            );
            let settled = match verdict {
                Some(outcome) => self.settle_run_from_verdict(&record, outcome, now),
                None => self.fail_run_after_restart(&record, now),
            };
            if settled {
                failed += 1;
            }
        }
        failed
    }

    /// Settles a run whose ending the store knows.
    ///
    /// A run whose terminal frame was published only needs its thread brought
    /// out of `working`; one whose frame never made it is finished normally, so
    /// the terminal exists exactly once.
    fn settle_run_from_verdict(&self, record: &RunRecord, outcome: RunOutcome, now: u64) -> bool {
        if record.terminal_published {
            return self.recover_published_terminal(record, outcome, now);
        }
        self.finish_run(record, outcome, record.failure_reason.clone(), now)
    }

    /// Writes the entity view to the store, with the log's watermark.
    ///
    /// The watermark is read *before* the entity view is copied. That ordering
    /// is the whole consistency argument: a mutation precedes the publish that
    /// records it, so any event at or below the watermark already happened when
    /// the view is copied. A mutation that raced ahead of the watermark can
    /// only make the snapshot fresher than the watermark, and replaying its
    /// event is idempotent.
    pub fn write_entity_view(&self) -> Result<(), persistence::SnapshotError> {
        let _run_lifecycle_guard = self.runs.lifecycle_lock();
        let _snapshot_guard = self
            .entity_write_lock
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let watermark = self
            .relay
            .high_watermark()
            .map_err(|error| persistence::SnapshotError::Io(error.to_string()))?;
        let snapshot = DomainSnapshot {
            version: SNAPSHOT_VERSION,
            watermark,
            registry: self.registry.export(),
            runs: self.runs.all(),
            settings: Some(self.settings.export()),
            automations: Some(self.automations.export()),
        };
        // One write, in one transaction. There is no file: the view's home is
        // the store, and a store from before that move is not a case this
        // server has.
        self.store()
            .replace_entities(&snapshot)
            .map_err(|error| persistence::SnapshotError::Io(error.to_string()))
    }

    /// Stops the writers, snapshots the entity view, and flushes the log.
    ///
    /// `&self` because every task is shared. The order is the contract, and
    /// each step is what makes the next one mean something:
    ///
    /// 1. **the periodic writers stop** (reconcile, schedule), so this server
    ///    produces nothing new of its own accord;
    /// 2. **the entity view is snapshotted** with the log's watermark. It runs
    ///    here, before the log is closed, so that anything published after it is
    ///    an event recovery will replay rather than a change missing from both
    ///    stores — and it takes the run lifecycle lock, which is what keeps the
    ///    watermark and the exported view consistent;
    /// 3. **the relay is closed**, so a task that wakes up late — a run
    ///    deadline, a retry timer — is refused instead of appending behind the
    ///    flush and leaving a tail the next process has to read around;
    /// 4. **the readers stop** (they only read, and freeing them is tidy);
    /// 5. **the store's writer is drained**, since it may be holding rows the
    ///    publish path accepted and no read can see otherwise;
    /// 6. **the log is drained and flushed**, and a failure is *returned*: a
    ///    flush that did not happen means the last write may not be on disk, and
    ///    an operator-driven stop can still fail its exit code for that.
    ///
    /// The snapshot is best-effort, for the reason it always was: it is a
    /// derived view with a periodic writer behind it, and the log is the source
    /// of truth. Its failure is reported and does not stop the flush.
    pub fn shutdown(&self) -> Result<(), ShutdownError> {
        self.reconcile_stop.store(true, Ordering::Relaxed);
        self.entity_write_stop.store(true, Ordering::Relaxed);
        self.schedule_stop.store(true, Ordering::Relaxed);
        if let Err(error) = self.write_entity_view() {
            eprintln!("loom-server: writing the entity view on shutdown failed: {error}");
        }
        self.relay.close();
        self.pump.stop();
        self.store_writer
            .flush()
            .map_err(|message| ShutdownError { message })?;
        // Last, and only once everything else is on disk: the store says this
        // stop finished, so a start that finds it unfinished knows the store may
        // be missing a tail rather than guessing.
        self.store()
            .mark_clean_stop()
            .map_err(|error| ShutdownError {
                message: format!("recording the store's clean stop failed: {error}"),
            })?;
        self.relay.flush().map_err(|error| ShutdownError {
            message: format!("flushing the relay log failed: {error}"),
        })
    }
}

/// Reads the domain event out of a stored relay frame, if it holds one.
///
/// A frame only counts when its payload parses as a [`DomainEvent`] whose own
/// scope is the frame's scope. Run dispatches and any raw producer payloads
/// share the log, so "is it JSON object with a `type` tag" is not enough.
pub(crate) fn domain_event_from_envelope(envelope: &loom_relay::Envelope) -> Option<DomainEvent> {
    let message: crate::protocol::WorkerServerMessage =
        serde_json::from_slice(&envelope.payload).ok()?;
    let crate::protocol::WorkerServerMessage::Event { payload, .. } = message else {
        return None;
    };
    let event: DomainEvent = serde_json::from_str(&payload).ok()?;
    (relay_scope(&event.scope()) == envelope.scope).then_some(event)
}

/// Maps a domain scope onto the relay scope that routes it.
///
/// The two types describe the same `(kind, id)` room, but they live in
/// different crates with a one-way dependency: the domain must not know the
/// relay exists, so the translation lives here, at the publish boundary.
pub fn relay_scope(scope: &DomainScope) -> loom_relay::Scope {
    match scope {
        DomainScope::Global => loom_relay::Scope::Global,
        DomainScope::Project(id) => loom_relay::Scope::Project(id.to_string()),
        DomainScope::Thread(id) => loom_relay::Scope::Thread(id.to_string()),
        DomainScope::Host(id) => loom_relay::Scope::Host(id.to_string()),
        DomainScope::User(id) => loom_relay::Scope::User(id.to_string()),
    }
}

impl std::fmt::Debug for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppState")
            .field("relay", &self.relay)
            .field("readers", &self.pump.reader_count())
            .finish_non_exhaustive()
    }
}

/// Test support for the public realtime channel.
///
/// Every invalidation test asserts the same thing — that a mutation published
/// one decodable `changed` frame — so the reading of that channel lives here
/// once rather than in each producer's test module.
#[cfg(test)]
pub(crate) mod realtime_test_support {
    use crate::pump::PublicRealtimeEvent;
    use tokio::sync::broadcast::Receiver;

    /// Waits for the project invalidation automations publish, decoded as JSON.
    ///
    /// Other entities' frames are stepped over: one request runs through the
    /// whole pipeline, so the environment, thread or host it touches publish
    /// their own changes alongside the automations one, and the assertion is
    /// about the automations frame arriving at all. A frame for a *different*
    /// project fails the test rather than being skipped.
    pub(crate) async fn expect_project_invalidation(
        events: &mut Receiver<PublicRealtimeEvent>,
        project: &str,
    ) -> serde_json::Value {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            assert!(
                !remaining.is_zero(),
                "no invalidation for project {project} was published"
            );
            let envelope = tokio::time::timeout(remaining, events.recv())
                .await
                .expect("a frame should be published")
                .expect("the channel stays open");
            let PublicRealtimeEvent::Envelope(envelope) = envelope else {
                panic!("a mutation must not reset public realtime");
            };
            let messages = crate::protocol::public_messages_from_frame(&envelope.payload);
            let Some(message) = messages.first() else {
                continue;
            };
            let frame = serde_json::to_value(message).expect("serializes");
            if frame.get("entity").and_then(|entity| entity.as_str()) != Some("project") {
                continue;
            }
            assert_eq!(
                frame,
                serde_json::json!({
                    "type": "changed",
                    "entity": "project",
                    "id": project,
                    "changes": ["project-updated"]
                })
            );
            return frame;
        }
    }

    /// Consumes frames until none has arrived for a moment.
    ///
    /// One operation can publish more than one — a manual run announces the run
    /// and then its settle when the dispatch had nowhere to go — so a test that
    /// asserts what the *next* operation publishes has to be looking at a quiet
    /// channel first.
    pub(crate) async fn drain(events: &mut Receiver<PublicRealtimeEvent>) {
        while let Ok(Ok(_)) =
            tokio::time::timeout(std::time::Duration::from_millis(150), events.recv()).await
        {
        }
    }

    /// Asserts nothing was published: the operation changed no client's view.
    pub(crate) async fn no_change(events: &mut Receiver<PublicRealtimeEvent>) {
        if let Ok(Ok(event)) =
            tokio::time::timeout(std::time::Duration::from_millis(200), events.recv()).await
        {
            panic!("an unchanged view was invalidated: {event:?}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use loom_domain::ThreadId;
    use loom_relay::Scope;
    use tempfile::TempDir;

    #[tokio::test]
    async fn publish_reaches_a_subscriber_end_to_end() {
        use loom_relay_hub::RecordingTransport;

        let state = AppState::build(AppConfig::default()).unwrap();
        let transport = RecordingTransport::new();
        let scope = Scope::Thread("thr_1".into());

        state
            .hub
            .connect(Box::new(transport.clone()), scope.clone())
            .await
            .unwrap();
        let envelope = state.publish(scope, "{\"n\":1}").unwrap();

        // Delivery is asynchronous: producer -> log -> reader -> actor -> sink.
        for _ in 0..200 {
            if !transport.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let frames = transport.frames();
        assert_eq!(frames.len(), 1);
        let frame: serde_json::Value = serde_json::from_str(&frames[0]).unwrap();
        assert_eq!(frame["type"], "event");
        assert_eq!(frame["payload"], "{\"n\":1}");
        assert_eq!(frame["event_id"], envelope.event_id.to_string());

        state.shutdown().unwrap();
    }

    /// A host's agents are offered while it reports them and dropped the moment
    /// it stops, so a record that died mid-probe cannot outlive the worker that
    /// was still assembling it.
    #[tokio::test]
    async fn a_hosts_providers_live_only_as_long_as_the_host_does() {
        let state = AppState::build(AppConfig::default()).unwrap();
        let host_id = loom_domain::HostId::mint();
        let found = ProviderSpec {
            name: "omp".into(),
            launch: loom_provider_protocol::ProviderLaunch::AcpStdio,
            command: "/usr/bin/omp".into(),
            args: vec!["acp".into()],
            cwd: None,
        };

        assert!(
            state.provider_spec_by_id("omp").is_none(),
            "nothing is offered before a host reports it"
        );
        state.record_host_providers(&host_id, vec![ProviderSpec::pi(), found.clone()]);
        assert_eq!(
            state.provider_spec_by_id("omp"),
            Some(found),
            "the report is what makes the agent dispatchable"
        );

        state.forget_host_providers(&host_id);
        assert!(
            state.provider_spec_by_id("omp").is_none(),
            "a disconnected host's agents stop being offered"
        );
        assert_eq!(
            state.provider_spec().name,
            "pi",
            "and the configured default is what remains"
        );

        state.shutdown().unwrap();
    }

    #[tokio::test]
    async fn a_published_event_is_replayable() {
        let state = AppState::build(AppConfig::default()).unwrap();
        let scope = Scope::Thread("thr_1".into());
        let envelope = state.publish(scope.clone(), "{\"n\":1}").unwrap();

        let replayed = state.relay.replay_scope(&scope, 10).unwrap();
        assert_eq!(replayed.len(), 1);
        assert_eq!(replayed[0].event_id, envelope.event_id);

        state.shutdown().unwrap();
    }

    #[tokio::test]
    async fn a_configured_data_directory_switches_to_the_durable_backend() {
        let dir = TempDir::new().unwrap();
        let config = AppConfig {
            backend_path: Some(dir.path().to_path_buf()),
            ..AppConfig::default()
        };
        let state = AppState::build(config).unwrap();
        let scope = Scope::Thread("thr_1".into());
        let envelope = state.publish(scope.clone(), "{\"n\":1}").unwrap();

        // The frames are in the store, and there is no shard file any more:
        // the database is the log's home.
        assert!(
            state.store().relay_event_total().unwrap() >= 1,
            "the frame reached the store"
        );
        assert!(
            !dir.path()
                .join(format!("shard-{}.log", scope.shard()))
                .exists(),
            "nothing writes the shard files"
        );

        let replayed = state.relay.replay_scope(&scope, 10).unwrap();
        assert_eq!(replayed.len(), 1);
        assert_eq!(replayed[0].event_id, envelope.event_id);

        state.shutdown().unwrap();
    }

    #[tokio::test]
    async fn reports_a_plausible_uptime() {
        let state = AppState::build(AppConfig::default()).unwrap();
        assert!(state.uptime_ms() < 10_000);
        assert!(state.started_at_ms() > 0);
    }

    #[tokio::test]
    async fn the_reader_count_is_fixed_by_shard_count() {
        let state = AppState::build(AppConfig::default()).unwrap();
        assert_eq!(
            state.pump.reader_count(),
            usize::from(loom_relay::SHARD_COUNT)
        );
    }

    #[test]
    fn every_domain_scope_maps_onto_a_relay_scope() {
        use loom_domain::{HostId, ProjectId, ThreadId, UserId};

        assert_eq!(relay_scope(&DomainScope::Global), Scope::Global);

        let project = ProjectId::mint();
        assert_eq!(
            relay_scope(&DomainScope::Project(project.clone())),
            Scope::Project(project.to_string())
        );
        let thread = ThreadId::mint();
        assert_eq!(
            relay_scope(&DomainScope::Thread(thread.clone())),
            Scope::Thread(thread.to_string())
        );
        let host = HostId::mint();
        assert_eq!(
            relay_scope(&DomainScope::Host(host.clone())),
            Scope::Host(host.to_string())
        );
        let user = UserId::mint();
        assert_eq!(
            relay_scope(&DomainScope::User(user.clone())),
            Scope::User(user.to_string())
        );
    }

    // ---- domain-state persistence ----

    /// A durable server that neither reconciles nor snapshots in the
    /// background, so a test drives both explicitly.
    fn durable_config(dir: &TempDir) -> AppConfig {
        AppConfig {
            backend_path: Some(dir.path().to_path_buf()),
            reconcile_interval: Duration::ZERO,
            entity_write_interval: Duration::ZERO,
            ..AppConfig::default()
        }
    }

    /// Clears the stored entity view, leaving only the log to recover from.
    ///
    /// This is what "there was no snapshot" means now: the view's home is the
    /// store, so a test that wants the log to be the only source empties it.
    /// The file is left where it is — nothing writes it any more, and a start
    /// with no stored view reads a legacy one if a test put it there.
    /// Removes a store's run rows, leaving the rest of the entity view.
    ///
    /// This is what a run the store never knew looks like with the thread's own
    /// state intact: the thread is still `working` and there is no record of
    /// what its run was doing.
    fn drop_stored_runs(dir: &TempDir) {
        let store = crate::store::Store::open(dir.path().join("loom.db")).unwrap();
        store
            .connection()
            .execute("DELETE FROM entity WHERE kind = 'run'", ())
            .unwrap();
    }

    fn drop_entity_view(dir: &TempDir) {
        let store = crate::store::Store::open(dir.path().join("loom.db")).unwrap();
        store
            .connection()
            .execute("DELETE FROM entity", ())
            .unwrap();
    }
    /// A run whose terminal frame was published is not published again.
    ///
    /// The verdict and the frame are both in the store when the process dies
    /// before the thread's status settles, so recovery finishes the *status*
    /// and leaves the terminal alone: a run that ended has one terminal event.
    #[tokio::test]
    async fn a_committed_terminal_is_not_published_again_during_recovery() {
        use loom_domain::{EnvironmentKind, MessageRole, RunEvent};

        let dir = TempDir::new().unwrap();
        let thread_id;
        let run_id;
        {
            let state = AppState::build(durable_config(&dir)).unwrap();
            let (host, host_event) = state
                .registry
                .enroll_host(None, "laptop".into(), now_ms())
                .unwrap();
            for event in &host_event {
                state.publish_domain_event(event).unwrap();
            }
            let (environment, environment_event) = state
                .registry
                .create_environment(
                    Some(state.registry.personal_project_id()),
                    host.id,
                    EnvironmentKind::Unmanaged,
                    Some("/srv/loom".into()),
                    now_ms(),
                )
                .unwrap();
            for event in &environment_event {
                state.publish_domain_event(event).unwrap();
            }
            let (thread, thread_event) = state
                .registry
                .create_thread(
                    Some(state.registry.personal_project_id()),
                    Some("terminal already committed".into()),
                    Some(environment.id),
                    now_ms(),
                )
                .unwrap();
            thread_id = thread.id.clone();
            state.publish_domain_event(&thread_event).unwrap();
            for event in state
                .registry
                .post_message(&thread_id, MessageRole::User, "hi".into(), now_ms())
                .unwrap()
            {
                state.publish_domain_event(&event).unwrap();
            }
            let thread = state.registry.thread(&thread_id).unwrap();
            let run = match state.dispatch_thread(&thread, "hi") {
                crate::runs::DispatchOutcome::Dispatched(run) => run,
                other => panic!("expected a dispatch, got {other:?}"),
            };
            run_id = run.run_id.clone();

            // The run path's own order, up to the crash: the start is marked,
            // the verdict is recorded, the frames are published, and the process
            // dies before it removes the record or publishes the thread's final
            // status.
            state
                .runs
                .mark_started(&run.run_id, "provider-1".to_owned());
            state
                .runs
                .mark_verdict(&run.run_id, loom_domain::RunOutcome::Completed);
            state
                .publish_domain_event(&DomainEvent::ThreadRunEvent {
                    run: Box::new(RunEvent::started(
                        thread.id.clone(),
                        thread.project_id.clone(),
                        run.run_id.clone(),
                        now_ms(),
                        "provider-1",
                    )),
                })
                .unwrap();
            state
                .publish_domain_event(&DomainEvent::ThreadRunEvent {
                    run: Box::new(RunEvent::completed(
                        thread.id,
                        thread.project_id,
                        run.run_id,
                        now_ms(),
                        Some("provider-1".into()),
                    )),
                })
                .unwrap();
            state.runs.mark_terminal(&run_id);
            state.shutdown().unwrap();
        }

        let state = AppState::build(durable_config(&dir)).unwrap();
        assert_eq!(
            state.registry.thread(&thread_id).unwrap().status,
            ThreadStatus::Idle
        );
        assert!(state.runs.is_empty());
        let run_events = state
            .relay
            .replay_scope(&Scope::Thread(thread_id.to_string()), 100)
            .unwrap()
            .into_iter()
            .filter_map(|envelope| domain_event_from_envelope(&envelope))
            .filter_map(|event| match event {
                DomainEvent::ThreadRunEvent { run } => Some(run),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            run_events.iter().map(|run| run.kind()).collect::<Vec<_>>(),
            vec!["turn/started", "turn/completed"]
        );
        assert!(run_events.iter().all(|run| run.run_id == run_id));
        state.shutdown().unwrap();
    }

    /// A run whose verdict the store knows but whose terminal frame never
    /// reached the log is finished on recovery — once.
    ///
    /// The verdict is written before the frame, so this is the reachable crash
    /// window: the run's ending is known, its frame may be missing, and
    /// recovery publishes what is missing instead of guessing from the log.
    #[tokio::test]
    async fn a_run_whose_frame_never_reached_the_log_is_finished_on_recovery() {
        use loom_domain::{EnvironmentKind, MessageRole, RunEvent};

        let dir = TempDir::new().unwrap();
        let thread_id;
        let run_id;
        {
            let state = AppState::build(durable_config(&dir)).unwrap();
            let (host, host_event) = state
                .registry
                .enroll_host(None, "laptop".into(), now_ms())
                .unwrap();
            for event in &host_event {
                state.publish_domain_event(event).unwrap();
            }
            let (environment, environment_event) = state
                .registry
                .create_environment(
                    Some(state.registry.personal_project_id()),
                    host.id,
                    EnvironmentKind::Unmanaged,
                    Some("/srv/loom".into()),
                    now_ms(),
                )
                .unwrap();
            for event in &environment_event {
                state.publish_domain_event(event).unwrap();
            }
            let (thread, thread_event) = state
                .registry
                .create_thread(
                    Some(state.registry.personal_project_id()),
                    Some("verdict without a frame".into()),
                    Some(environment.id),
                    now_ms(),
                )
                .unwrap();
            thread_id = thread.id.clone();
            state.publish_domain_event(&thread_event).unwrap();
            for event in state
                .registry
                .post_message(&thread_id, MessageRole::User, "hi".into(), now_ms())
                .unwrap()
            {
                state.publish_domain_event(&event).unwrap();
            }
            let thread = state.registry.thread(&thread_id).unwrap();
            let run = match state.dispatch_thread(&thread, "hi") {
                crate::runs::DispatchOutcome::Dispatched(run) => run,
                other => panic!("expected a dispatch, got {other:?}"),
            };
            run_id = run.run_id.clone();
            state
                .publish_domain_event(&DomainEvent::ThreadRunEvent {
                    run: Box::new(RunEvent::started(
                        thread.id.clone(),
                        thread.project_id.clone(),
                        run.run_id.clone(),
                        now_ms(),
                        "provider-1",
                    )),
                })
                .unwrap();
            // What the run path does with a start frame, and then the verdict:
            // the process dies before the terminal frame is appended.
            state.runs.mark_started(&run_id, "provider-1".to_owned());
            state
                .runs
                .mark_verdict(&run_id, loom_domain::RunOutcome::Completed);
            state.shutdown().unwrap();
        }

        let state = AppState::build(durable_config(&dir)).unwrap();
        assert_eq!(
            state.registry.thread(&thread_id).unwrap().status,
            ThreadStatus::Idle
        );
        let run_events = state
            .relay
            .replay_scope(&Scope::Thread(thread_id.to_string()), 100)
            .unwrap()
            .into_iter()
            .filter_map(|envelope| domain_event_from_envelope(&envelope))
            .filter_map(|event| match event {
                DomainEvent::ThreadRunEvent { run } => Some(run),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            run_events.iter().map(|run| run.kind()).collect::<Vec<_>>(),
            vec!["turn/started", "turn/completed"],
            "recovery publishes the terminal the run never got to"
        );
        assert!(run_events.iter().all(|run| run.run_id == run_id));
        state.shutdown().unwrap();
    }

    /// A thread left `working` with no stored run is failed rather than guessed
    /// at.
    ///
    /// This is the other side of the window the verdict closes: with no record
    /// of the run there is nothing to say how it ended, so the thread is brought
    /// out of `working` with a failure instead of a terminal nobody recorded.
    #[tokio::test]
    async fn a_working_thread_with_no_stored_run_is_failed_on_recovery() {
        let dir = TempDir::new().unwrap();
        let thread_id;
        {
            let state = AppState::build(durable_config(&dir)).unwrap();
            let (thread, created) = state
                .registry
                .create_thread(
                    Some(state.registry.personal_project_id()),
                    Some("no record".into()),
                    None,
                    now_ms(),
                )
                .unwrap();
            thread_id = thread.id.clone();
            state.publish_domain_event(&created).unwrap();
            // Posting a message is what puts a thread into `working`, and it
            // publishes the status change with it.
            for event in state
                .registry
                .post_message(
                    &thread_id,
                    loom_domain::MessageRole::User,
                    "hi".into(),
                    now_ms(),
                )
                .unwrap()
            {
                state.publish_domain_event(&event).unwrap();
            }
            assert_eq!(
                state.registry.thread(&thread_id).unwrap().status,
                ThreadStatus::Working
            );
            state.shutdown().unwrap();
        }

        // The run, and only the run, is lost: the thread is still `working`
        // with nothing to say what its run did.
        drop_stored_runs(&dir);
        let state = AppState::build(durable_config(&dir)).unwrap();
        assert_eq!(
            state.registry.thread(&thread_id).unwrap().status,
            ThreadStatus::Error,
            "a run nothing recorded is failed, not left working"
        );
        state.shutdown().unwrap();
    }

    /// A clean shutdown leaves a log whose tail the next process can read.
    ///
    /// The property is deliberately about the *flush*, not about `Drop`: the
    /// state that wrote the log is still alive when the next one opens the same
    /// directory, so a backend that relied on being dropped would leave the
    /// test reading a file with unfinished writes in it. A late publish through
    /// the old state is refused rather than landing behind the flush.
    /// The store is opened with the server, in the same directory the rest of
    /// its data lives in.
    #[tokio::test]
    async fn the_store_lives_in_the_data_directory() {
        let dir = TempDir::new().unwrap();
        let state = AppState::build(durable_config(&dir)).unwrap();
        assert!(dir.path().join("loom.db").exists(), "the store is a file");
        assert_eq!(
            state.store().schema_version().unwrap(),
            crate::store::SCHEMA_VERSION
        );
        state.shutdown().unwrap();
    }

    /// A temporary server still has a store — with the same schema, in memory.
    /// "No data directory" is nowhere to keep history, not permission to run
    /// without it.
    #[tokio::test]
    async fn a_temporary_server_still_has_a_store() {
        let state = AppState::build(AppConfig::default()).unwrap();
        assert_eq!(
            state.store().schema_version().unwrap(),
            crate::store::SCHEMA_VERSION
        );
        state.shutdown().unwrap();
    }

    /// The entity view comes back without the file, and without the log.
    ///
    /// This is what the move is for: the view used to be a file plus a replay of
    /// whatever the bounded log still held, so a thread whose creation had been
    /// evicted was a thread a restart could lose. Here the file is deleted, the
    /// log is too small to hold the event, and the thread is still there.
    #[tokio::test]
    async fn the_entity_view_comes_back_without_the_file_or_the_log() {
        let dir = TempDir::new().unwrap();
        let mut config = durable_config(&dir);
        config.backend_max_len = 2;
        let thread_id;
        {
            let state = AppState::build(config.clone()).unwrap();
            let (thread, created) = state
                .registry
                .create_thread(
                    Some(state.registry.personal_project_id()),
                    Some("only in the store".into()),
                    None,
                    now_ms(),
                )
                .unwrap();
            thread_id = thread.id.clone();
            state.publish_domain_event(&created).unwrap();
            state.write_entity_view().unwrap();
            // Push the creation out of the retained window.
            for frame in 0..4 {
                state
                    .publish(
                        loom_relay::Scope::Thread(thread_id.to_string()),
                        format!("{{\"filler\":{frame}}}"),
                    )
                    .unwrap();
            }
            state.shutdown().unwrap();
        }
        // There is no file to remove: the store is where the view is, and the
        // test above pushed the log's copy out of its window.

        let state = AppState::build(config).unwrap();
        let thread = state
            .registry
            .thread(&thread_id)
            .expect("the thread came back from the store, not from the log or the file");
        assert_eq!(thread.title.as_deref(), Some("only in the store"));
        state.shutdown().unwrap();
    }

    /// A run's state is durable as it changes, not only at the periodic write.
    ///
    /// A restart settles in-flight runs from these records, so the flags have to
    /// be there before the effect they guard: a run inserted and started is in
    /// the store, and a run that leaves flight is gone.
    #[tokio::test]
    async fn a_run_state_is_written_when_it_changes() {
        let dir = TempDir::new().unwrap();
        let state = AppState::build(durable_config(&dir)).unwrap();
        let run_id = loom_domain::RunId::mint();
        state.runs.insert(crate::runs::RunRecord {
            run_id: run_id.clone(),
            thread_id: loom_domain::ThreadId::mint(),
            project_id: loom_domain::ProjectId::sentinel().unwrap(),
            host_id: loom_domain::HostId::mint(),
            cwd: "/srv/project".to_owned(),
            started_at_ms: 10,
            deadline_ms: 20,
            last_event_ms: None,
            open_items: 0,
            turn_started: false,
            provider_thread_id: None,
            provider_id: None,
            provider_error_reported: false,
            failure_reason: None,
            terminal_published: false,
            terminal_outcome: None,
            pending_status_event: None,
        });
        assert_eq!(state.store().runs().unwrap().len(), 1);

        state.runs.mark_started(&run_id, "acp-session-1".to_owned());
        let stored = state.store().runs().unwrap();
        assert!(
            stored[0].turn_started && stored[0].provider_thread_id.is_some(),
            "the start is on disk before the effect it guards: {stored:?}"
        );

        state.runs.remove(&run_id);
        assert!(state.store().runs().unwrap().is_empty());
        state.shutdown().unwrap();
    }

    /// A stop that did not finish is visible in the next process's read.
    ///
    /// The kill cannot be simulated by a signal here, but it can be by what the
    /// file is left saying: the previous state is dropped without its shutdown,
    /// which is exactly the mark a killed process leaves, and the next server
    /// must not call the conversation it finds complete.
    #[tokio::test]
    async fn a_conversation_that_outlived_an_unfinished_stop_is_not_complete() {
        let dir = TempDir::new().unwrap();
        let thread_id;
        {
            let state = AppState::build(durable_config(&dir)).unwrap();
            let (thread, created) = state
                .registry
                .create_thread(
                    Some(state.registry.personal_project_id()),
                    Some("killed".into()),
                    None,
                    now_ms(),
                )
                .unwrap();
            state.publish_domain_event(&created).unwrap();
            thread_id = thread.id.clone();
            let first = state.seqs().reserve(&thread_id, 1);
            state
                .store()
                .replace_replayed(
                    &thread_id,
                    &crate::history_cache::CacheBinding {
                        host_id: loom_domain::HostId::mint(),
                        agent: "pi".to_owned(),
                        provider_session_id: "acp-session-1".to_owned(),
                        cwd: "/srv/project".to_owned(),
                    },
                    first,
                    &[],
                    now_ms(),
                )
                .unwrap();
            // Deliberately no `shutdown()`: this process is "killed".
            std::mem::forget(state);
        }

        let state = AppState::build(durable_config(&dir)).unwrap();
        let view = state.stored_view(&thread_id).unwrap();
        assert!(
            !view.complete,
            "a conversation from an unfinished stop is not complete: {view:?}"
        );
        assert_eq!(view.status, crate::history_cache::HistoryStatus::Stale);
        assert!(
            view.reason
                .as_deref()
                .is_some_and(|reason| reason.contains("stopped without finishing")),
            "the reason says what happened: {view:?}"
        );
        state.shutdown().unwrap();

        // A stop that finishes does not clear the warning: restarting is not the
        // same as learning what was lost, and only a load that succeeds can say
        // the conversation is whole again.
        let state = AppState::build(durable_config(&dir)).unwrap();
        let still_marked = state.stored_view(&thread_id).unwrap();
        assert!(
            !still_marked.complete
                && still_marked
                    .reason
                    .as_deref()
                    .is_some_and(|reason| reason.contains("stopped without finishing")),
            "the warning outlives a clean restart: {still_marked:?}"
        );

        // What clears it is the conversation being loaded again.
        let binding = crate::history_cache::CacheBinding {
            host_id: loom_domain::HostId::mint(),
            agent: "pi".to_owned(),
            provider_session_id: "acp-session-1".to_owned(),
            cwd: "/srv/project".to_owned(),
        };
        let first = state.seqs().reserve(&thread_id, 0);
        state
            .store()
            .replace_replayed(&thread_id, &binding, first, &[], now_ms())
            .unwrap();
        assert!(
            state.stored_view(&thread_id).unwrap().complete,
            "a load that succeeds is what says the conversation is whole"
        );
        state.shutdown().unwrap();
    }

    /// A conversation row reaches the store from the publish seam, and the
    /// publisher does not wait for the disk to get there.
    #[tokio::test]
    async fn a_published_message_reaches_the_store() {
        let state = AppState::build(AppConfig::default()).unwrap();
        let (thread, created) = state
            .registry
            .create_thread(
                Some(state.registry.personal_project_id()),
                Some("stored".into()),
                None,
                now_ms(),
            )
            .unwrap();
        state.publish_domain_event(&created).unwrap();
        for event in state
            .registry
            .post_message(
                &thread.id,
                loom_domain::MessageRole::User,
                "hello".into(),
                now_ms(),
            )
            .unwrap()
        {
            state.publish_domain_event(&event).unwrap();
        }

        assert!(
            state
                .store_writer()
                .wait_for_writes(1, Duration::from_secs(2)),
            "the row reaches the store"
        );
        let store = state.store();
        assert_eq!(store.row_count(&thread.id).unwrap(), 1);
        let rows = store.rows(&thread.id).unwrap();
        assert!(
            matches!(
                rows[0].source,
                crate::history_cache::RowSource::Message { .. }
            ),
            "a message keeps its source: {:?}",
            rows[0].source
        );
        assert_eq!(state.store_writer().unsaved(&thread.id), None);
        drop(store);
        state.shutdown().unwrap();
    }

    /// A backlog the publish path accepted is written by the time shutdown
    /// returns, and a later server reads it back.
    #[tokio::test]
    async fn a_shutdown_drains_the_store_backlog() {
        let dir = TempDir::new().unwrap();
        let state = AppState::build(durable_config(&dir)).unwrap();
        let (thread, created) = state
            .registry
            .create_thread(
                Some(state.registry.personal_project_id()),
                Some("drained".into()),
                None,
                now_ms(),
            )
            .unwrap();
        state.publish_domain_event(&created).unwrap();
        let first = thread.id.clone();
        let mut expected = 0;
        for index in 0..25 {
            let events = state
                .registry
                .post_message(
                    &first,
                    loom_domain::MessageRole::User,
                    format!("message {index}"),
                    now_ms(),
                )
                .unwrap();
            for event in events {
                if matches!(event, loom_domain::DomainEvent::ThreadMessageAdded { .. }) {
                    expected += 1;
                }
                state.publish_domain_event(&event).unwrap();
            }
        }
        // Deliberately no wait: the point is that the stop itself drains.
        state.shutdown().unwrap();

        let store = crate::store::Store::open(dir.path().join("loom.db")).unwrap();
        assert_eq!(
            store.row_count(&first).unwrap(),
            expected,
            "every accepted row is on disk after shutdown"
        );
    }

    /// A store that cannot be opened fails startup rather than being skipped.
    #[tokio::test]
    async fn a_store_that_cannot_be_opened_fails_startup() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("loom.db"), b"not a database").unwrap();
        let error = AppState::build(durable_config(&dir)).unwrap_err();
        assert!(
            error.to_string().contains("store"),
            "the failure names the store: {error}"
        );
    }

    #[tokio::test]
    async fn a_shutdown_log_tail_is_readable_by_a_later_server() {
        let dir = TempDir::new().unwrap();
        let first = AppState::build(durable_config(&dir)).unwrap();

        let (thread, created) = first
            .registry
            .create_thread(
                Some(first.registry.personal_project_id()),
                Some("a long conversation".into()),
                None,
                now_ms(),
            )
            .unwrap();
        first.publish_domain_event(&created).unwrap();
        let scope = Scope::Thread(thread.id.to_string());
        for index in 0..64 {
            first
                .publish(scope.clone(), format!("{{\"fill\":{index}}}"))
                .unwrap();
        }
        // A last frame large enough that the writer cannot have finished it by
        // the time the server is told to stop: without a flush that drains, the
        // log this test reopens is missing its tail.
        first
            .publish(scope.clone(), "x".repeat(16 * 1024 * 1024))
            .unwrap();
        let stored = first.relay.replay_scope(&scope, 1_000).unwrap().len();
        assert_eq!(stored, 65);

        first.shutdown().unwrap();

        // The old relay refuses to write after the flush: a task that woke up
        // late must not be able to leave a tail.
        assert!(first.relay.is_closed());
        assert!(
            first.publish(scope.clone(), "{\"late\":true}").is_err(),
            "a shutdown server must not accept another write"
        );

        // A later server over the same directory, while the first is still
        // alive and undropped.
        let second = AppState::build(durable_config(&dir)).unwrap();
        let replayed = second.relay.replay_scope(&scope, 1_000).unwrap();
        assert_eq!(
            replayed.len(),
            stored,
            "every frame the first server accepted must be readable"
        );
        let restored = second
            .registry
            .thread(&thread.id)
            .expect("the entity view came from the snapshot");
        assert_eq!(restored.title.as_deref(), Some("a long conversation"));

        second.shutdown().unwrap();
        drop(first);
    }

    /// A flush that fails is *returned*, not printed.
    ///
    /// The process is exiting because it was told to, and the only useful thing
    /// it can still do with a durability failure is fail its exit code — so a
    /// shutdown that could not flush must say so.
    #[tokio::test]
    async fn a_flush_failure_fails_the_shutdown() {
        /// A backend whose writes never reach the disk.
        struct Unflushable(loom_relay::backend::memory::MemoryBackend);

        impl loom_relay::RelayBackend for Unflushable {
            fn shard_count(&self) -> u8 {
                self.0.shard_count()
            }
            fn append(
                &self,
                shard: loom_relay::ShardId,
                record: loom_relay::LogRecord,
            ) -> loom_relay::Result<()> {
                self.0.append(shard, record)
            }
            fn read_after(
                &self,
                shard: loom_relay::ShardId,
                after: Option<loom_relay::EventId>,
                limit: usize,
            ) -> loom_relay::Result<Vec<loom_relay::LogRecord>> {
                self.0.read_after(shard, after, limit)
            }
            fn trim(&self, shard: loom_relay::ShardId, before_ms: u64) -> loom_relay::Result<u64> {
                self.0.trim(shard, before_ms)
            }
            fn len(&self, shard: loom_relay::ShardId) -> loom_relay::Result<usize> {
                self.0.len(shard)
            }
            fn flush(&self) -> loom_relay::Result<()> {
                Err(loom_relay::RelayError::backend("the disk went away"))
            }
        }

        let state = AppState::build_for_test(
            AppConfig {
                reconcile_interval: Duration::ZERO,
                schedule_interval: Duration::ZERO,
                entity_write_interval: Duration::ZERO,
                ..AppConfig::default()
            },
            Arc::new(Unflushable(
                loom_relay::backend::memory::MemoryBackend::new(64),
            )),
        )
        .unwrap();

        let error = state
            .shutdown()
            .expect_err("a failed flush must fail the shutdown");
        assert!(error.to_string().contains("the disk went away"), "{error}");
    }

    #[tokio::test]
    async fn a_log_without_a_snapshot_rebuilds_without_panicking() {
        let dir = TempDir::new().unwrap();
        let thread_id;
        {
            let state = AppState::build(durable_config(&dir)).unwrap();
            let (thread, created) = state
                .registry
                .create_thread(
                    Some(state.registry.personal_project_id()),
                    Some("t".into()),
                    None,
                    now_ms(),
                )
                .unwrap();
            thread_id = thread.id.clone();
            // Publishing the creation event is what puts the entity in the log;
            // the registry call alone only mutates memory.
            state.publish_domain_event(&created).unwrap();
            // An event for an entity that was never created. Recovery must
            // ignore it rather than panic or invent the entity.
            state
                .publish_domain_event(&DomainEvent::ThreadStatusChanged {
                    thread_id: ThreadId::mint(),
                    project_id: state.registry.personal_project_id(),
                    from: ThreadStatus::Idle,
                    to: ThreadStatus::Working,
                    at_ms: now_ms(),
                })
                .unwrap();
            state.shutdown().unwrap();
        }

        // Drop the view: the retained log is the only surviving source.
        drop_entity_view(&dir);

        let state = AppState::build(durable_config(&dir)).unwrap();
        let rebuilt = state
            .registry
            .thread(&thread_id)
            .expect("the created thread is rebuilt");
        assert_eq!(rebuilt.title.as_deref(), Some("t"));
        // The orphan event neither panicked nor minted a second thread.
        assert_eq!(state.registry.threads().len(), 1);
        state.shutdown().unwrap();
    }

    /// The store is the only place a view is read from, so a request for one
    /// that was never written rebuilds from the log and says so — no file is
    /// consulted and none is written.
    #[tokio::test]
    async fn a_view_that_was_never_written_is_rebuilt_from_the_log() {
        let dir = TempDir::new().unwrap();
        let thread_id;
        {
            let state = AppState::build(durable_config(&dir)).unwrap();
            let (thread, created) = state
                .registry
                .create_thread(
                    Some(state.registry.personal_project_id()),
                    Some("t".into()),
                    None,
                    now_ms(),
                )
                .unwrap();
            thread_id = thread.id.clone();
            state.publish_domain_event(&created).unwrap();
            state.shutdown().unwrap();
        }
        drop_entity_view(&dir);
        // Nothing in the data directory is a file the server reads back: what
        // survives is the database the store is.
        assert!(
            !dir.path().join("domain.snapshot").exists(),
            "no snapshot file is written, and none is read"
        );

        let state = AppState::build(durable_config(&dir)).unwrap();
        assert!(state.registry.thread(&thread_id).is_some());
        state.shutdown().unwrap();
    }

    #[tokio::test]
    async fn a_snapshot_is_not_written_without_a_data_directory() {
        let state = AppState::build(AppConfig::default()).unwrap();
        assert!(state.write_entity_view().is_ok());
        state.shutdown().unwrap();
    }
}
