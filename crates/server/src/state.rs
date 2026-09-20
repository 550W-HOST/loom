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

use loom_domain::{DomainEvent, DomainScope, HostId, RunId, RunOutcome, ThreadId, ThreadStatus};
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
    /// How long a dispatched run may stay in flight before the server reaps it.
    ///
    /// This is the backstop for a worker that is connected but wedged. The
    /// execution plane enforces its own provider timeout; this one exists so a
    /// silent worker cannot leave a thread `working` forever.
    pub run_timeout: Duration,
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
    /// How often a domain snapshot is written to disk.
    ///
    /// Only meaningful when [`AppConfig::backend_path`] names a data
    /// directory: the in-process default keeps no domain state, so it has no
    /// local snapshot to write.
    /// `Duration::ZERO` disables the periodic writer; a snapshot is still
    /// written once, on a clean shutdown.
    pub snapshot_interval: Duration,
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
            host_stale_after: Duration::from_secs(60),
            reconcile_interval: Duration::from_secs(5),
            schedule_interval: Duration::from_secs(10),
            snapshot_interval: Duration::from_secs(30),
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
    /// The control plane's terminal session index.
    pub terminals: Arc<crate::terminals::TerminalSessions>,
    local_host_id: Option<HostId>,
    run_timeout_ms: u64,
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
    snapshot_stop: Arc<AtomicBool>,
    schedule_stop: Arc<AtomicBool>,
    snapshot_lock: Arc<Mutex<()>>,
    snapshot_root: Option<PathBuf>,
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
    pub fn store(&self) -> std::sync::MutexGuard<'_, crate::store::Store> {
        self.store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Wires the relay, hub actor and readers together.
    pub fn build(config: AppConfig) -> Result<Self, BuildStateError> {
        let backend: loom_relay::SharedBackend = match &config.backend_path {
            // Durable backend: replay survives a restart on this machine.
            Some(path) => Arc::new(loom_relay::backend::disk::DiskBackend::open(
                path,
                config.backend_max_len,
            )?),
            // Default: in-process, zero external service.
            None => Arc::new(loom_relay::backend::memory::MemoryBackend::new(
                config.backend_max_len,
            )),
        };
        Self::build_from_backend(config, backend)
    }

    #[cfg(test)]
    pub(crate) fn build_for_test(
        config: AppConfig,
        backend: loom_relay::SharedBackend,
    ) -> Result<Self, BuildStateError> {
        Self::build_from_backend(config, backend)
    }

    fn build_from_backend(
        config: AppConfig,
        backend: loom_relay::SharedBackend,
    ) -> Result<Self, BuildStateError> {
        let store = match &config.backend_path {
            Some(path) => crate::store::Store::open(path.join("loom.db"))?,
            None => crate::store::Store::open_in_memory()?,
        };
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

        // Domain-state persistence rides on the durable backend: it is the
        // deployment where a restart has a log to recover from. The in-process
        // backend leaves the entity view ephemeral (see
        // `docs/domain-persistence.md`).
        let snapshot_root = config.backend_path.clone();
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
            runs: Arc::new(RunRegistry::new()),
            settings: Arc::new(SettingsRegistry::new(&provider_id)),
            automations: Arc::new(AutomationsRegistry::new()),
            ui,
            artifacts,
            file_previews: Arc::new(FilePreviewRegistry::new()),
            join_codes: Arc::new(JoinCodeRegistry::new()),
            host_files: Arc::new(HostFileBroker::new()),
            host_rpc: Arc::new(HostRpcBroker::new()),
            history_rpc: Arc::new(HistoryBroker::new()),
            history: Arc::new(crate::history_cache::HistoryCache::new(
                HISTORY_CACHE_THREADS,
                HISTORY_CACHE_BYTES,
                HISTORY_CACHE_CONCURRENT_LOADS,
            )),
            store: Arc::new(Mutex::new(store)),
            history_waits: Arc::new(crate::history::HistoryWaits::new()),
            terminal: Arc::new(crate::terminals::TerminalBroker::new()),
            catalogs: Arc::new(crate::catalogs::CatalogRegistry::new()),
            terminals: Arc::new(crate::terminals::TerminalSessions::new()),
            local_host_id: config.local_host_id,
            run_timeout_ms: config.run_timeout.as_millis().min(u128::from(u64::MAX)) as u64,
            host_stale_after_ms: config
                .host_stale_after
                .as_millis()
                .min(u128::from(u64::MAX)) as u64,
            provider_specs,
            host_providers: Arc::new(Mutex::new(Vec::new())),
            reconcile_stop: Arc::new(AtomicBool::new(false)),
            snapshot_stop: Arc::new(AtomicBool::new(false)),
            schedule_stop: Arc::new(AtomicBool::new(false)),
            snapshot_lock: Arc::new(Mutex::new(())),
            snapshot_root,
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
        if !config.snapshot_interval.is_zero() && state.snapshot_root.is_some() {
            state.spawn_snapshotter(config.snapshot_interval);
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
            if let Err(error) = self.snapshot() {
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

    /// Starts the periodic domain-snapshot writer.
    fn spawn_snapshotter(&self, interval: Duration) {
        let state = self.clone();
        let stop = Arc::clone(&self.snapshot_stop);
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
                if let Err(error) = state.snapshot() {
                    eprintln!("loom-server: periodic domain snapshot failed: {error}");
                }
            }
        });
    }

    /// How long a run may stay in flight before it is reaped.
    pub(crate) fn run_timeout_ms(&self) -> u64 {
        self.run_timeout_ms
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
        self.history
            .append_live(&thread_id, binding.as_ref(), source, body);
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
        let Some(root) = &self.snapshot_root else {
            return;
        };
        let now = now_ms();
        match persistence::read_snapshot(root) {
            Ok(Some(snapshot)) => {
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
                    "loom-server: restored the domain snapshot ({replayed} log events replayed, \
                     {failed} in-flight runs failed)"
                );
            }
            Ok(None) => {
                // No snapshot yet: the retained log is the only thing to go on.
                let replayed = self.replay_domain_events(None);
                let failed = self.fail_in_flight_runs(Vec::new(), now);
                if replayed > 0 || failed > 0 {
                    eprintln!(
                        "loom-server: no domain snapshot; rebuilt {replayed} events from the log \
                         ({failed} in-flight runs failed)"
                    );
                }
            }
            Err(error) => {
                eprintln!(
                    "loom-server: domain snapshot unusable ({error}); rebuilding from the log"
                );
                let replayed = self.replay_domain_events(None);
                let failed = self.fail_in_flight_runs(Vec::new(), now);
                eprintln!(
                    "loom-server: rebuilt {replayed} events from the log ({failed} in-flight runs \
                     failed)"
                );
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

    /// Fails every run that was in flight when the previous process stopped.
    ///
    /// A restarted server cannot prove a provider is still running, so the
    /// invariant it protects instead is that no thread is left `working` and no
    /// run is left without a terminal event. The worker's later report is an
    /// idempotent no-op (`ReportOutcome::Unknown`). Returns how many runs were
    /// failed.
    fn fail_in_flight_runs(&self, records: Vec<RunRecord>, now: u64) -> usize {
        let mut restored = Vec::new();
        for mut record in records {
            let terminal = self.recover_run_flags(&mut record);
            let Some(thread) = self.registry.thread(&record.thread_id) else {
                continue;
            };
            if terminal.is_none()
                && !matches!(thread.status, ThreadStatus::Working | ThreadStatus::Waiting)
            {
                continue;
            }
            restored.push((record, terminal));
        }
        self.runs
            .restore(restored.iter().map(|(record, _)| record.clone()));

        let mut failed = 0;
        for (record, terminal) in &restored {
            let settled = match terminal {
                Some(outcome) => self.recover_published_terminal(record, *outcome, now),
                None => self.fail_run_after_restart(record, now),
            };
            if settled {
                failed += 1;
            }
        }
        // A run dispatched after the last snapshot has no record here, but its
        // thread is still `working`. Sweep those too, using the run id the
        // thread recorded when it was dispatched.
        for thread in self.registry.threads() {
            if !matches!(thread.status, ThreadStatus::Working | ThreadStatus::Waiting) {
                continue;
            }
            let (record, terminal) = self.runs.for_thread(&thread.id).map_or_else(
                || {
                    let run_id = thread
                        .active_run_id
                        .clone()
                        .or_else(|| self.latest_active_run_id(&thread.id))
                        .unwrap_or_else(RunId::mint);
                    let mut record = RunRecord {
                        run_id,
                        thread_id: thread.id.clone(),
                        project_id: thread.project_id.clone(),
                        host_id: HostId::mint(),
                        cwd: String::new(),
                        started_at_ms: now,
                        deadline_ms: now,
                        turn_started: false,
                        provider_thread_id: None,
                        provider_id: None,
                        provider_error_reported: false,
                        failure_reason: None,
                        terminal_published: false,
                        terminal_outcome: None,
                        pending_status_event: None,
                    };
                    let terminal = self.recover_run_flags(&mut record);
                    self.runs.insert(record.clone());
                    (record, terminal)
                },
                |record| {
                    let mut record = record;
                    let terminal = self.recover_run_flags(&mut record);
                    (record, terminal)
                },
            );
            let settled = match terminal {
                Some(outcome) => self.recover_published_terminal(&record, outcome, now),
                None => self.fail_run_after_restart(&record, now),
            };
            if settled {
                failed += 1;
            }
        }
        failed
    }

    /// Finds the last run event after the current thread entered `working`.
    ///
    /// A terminal run event clears `Thread::active_run_id` while replaying, so
    /// a thread can still be `working` with no run record or active id when a
    /// snapshot was taken before the terminal append. The status transition is
    /// the durable boundary that lets recovery distinguish that terminal from
    /// a previous turn's terminal event.
    ///
    /// This asks the **log**, not the timeline cache: "did this thread's turn
    /// finish" is loom's own fact, and an agent's session replay cannot answer
    /// it. It is therefore still bounded by whatever the backend retains — a
    /// burst larger than `backend_max_len` can evict the boundary event and
    /// make this decide wrongly. That is a recovery-correctness limitation
    /// tracked separately from the conversation cache; see
    /// `docs/architecture.md` § The conversation is not in the log.
    fn latest_active_run_id(&self, thread_id: &ThreadId) -> Option<RunId> {
        let scope = Scope::Thread(thread_id.to_string());
        // Recovery reads history, so it must not be bounded by the replay
        // window: after a downtime longer than the grace window, a windowed
        // read would find no boundary and mis-recover the run.
        let Ok(envelopes) = self.relay.retained_scope(&scope, usize::MAX) else {
            return None;
        };
        let mut active = false;
        let mut run_id = None;
        for envelope in envelopes {
            let Some(event) = domain_event_from_envelope(&envelope) else {
                continue;
            };
            match event {
                DomainEvent::ThreadStatusChanged {
                    to: ThreadStatus::Working,
                    ..
                } => {
                    active = true;
                    run_id = None;
                }
                DomainEvent::ThreadStatusChanged {
                    to: ThreadStatus::Idle | ThreadStatus::Error | ThreadStatus::Archived,
                    ..
                } => {
                    active = false;
                    run_id = None;
                }
                DomainEvent::ThreadRunEvent { run } if active => {
                    run_id = Some(run.run_id.clone());
                }
                _ => {}
            }
        }
        run_id
    }

    /// Reconciles lifecycle progress for a run in an older or concurrently
    /// captured snapshot with the retained relay log. Returns the terminal
    /// outcome when that terminal has already been committed.
    ///
    /// Like [`AppState::latest_active_run_id`], this reads loom's own run
    /// events rather than the conversation cache, and is bounded by what the
    /// backend still retains: a terminal evicted past the shard cap is a run
    /// this cannot settle from the log.
    fn recover_run_flags(&self, record: &mut RunRecord) -> Option<RunOutcome> {
        let scope = Scope::Thread(record.thread_id.to_string());
        let Ok(envelopes) = self.relay.retained_scope(&scope, usize::MAX) else {
            return record.terminal_outcome;
        };
        let mut terminal = None;
        let pending_status_event = record.pending_status_event.clone();
        let mut pending_status_published = false;
        for envelope in envelopes {
            let Some(event) = domain_event_from_envelope(&envelope) else {
                continue;
            };
            match event {
                DomainEvent::ThreadStatusChanged { .. } => {
                    if pending_status_event
                        .as_ref()
                        .is_some_and(|pending| pending.matches_event(&event))
                    {
                        pending_status_published = true;
                    }
                }
                DomainEvent::ThreadRunEvent { run }
                    if run.run_id == record.run_id && run.thread_id == record.thread_id =>
                {
                    match run.kind() {
                        "turn/started" => {
                            record.turn_started = true;
                            record.provider_thread_id = run.provider_thread_id().map(str::to_owned);
                        }
                        "provider/error" => {
                            record.provider_error_reported = true;
                            if record.provider_thread_id.is_none() {
                                record.provider_thread_id =
                                    run.provider_thread_id().map(str::to_owned);
                            }
                        }
                        "turn/completed" if terminal.is_none() => {
                            terminal = Some(run.terminal_outcome().unwrap_or(RunOutcome::Failed));
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
        if pending_status_published {
            record.pending_status_event = None;
        }
        if let Some(outcome) = terminal {
            record.terminal_published = true;
            record.terminal_outcome = Some(outcome);
        }
        terminal.or(record.terminal_outcome)
    }

    /// Writes a domain snapshot to the data directory, if one is configured.
    ///
    /// The watermark is read *before* the entity view is copied. That ordering
    /// is the whole consistency argument: a mutation precedes the publish that
    /// records it, so any event at or below the watermark already happened when
    /// the view is copied. A mutation that raced ahead of the watermark can
    /// only make the snapshot fresher than the watermark, and replaying its
    /// event is idempotent.
    pub fn snapshot(&self) -> Result<(), persistence::SnapshotError> {
        let Some(root) = &self.snapshot_root else {
            return Ok(());
        };
        let _run_lifecycle_guard = self.runs.lifecycle_lock();
        let _snapshot_guard = self
            .snapshot_lock
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
        persistence::write_snapshot(root, &snapshot)
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
    /// 5. **the log is drained and flushed**, and a failure is *returned*: a
    ///    flush that did not happen means the last write may not be on disk, and
    ///    an operator-driven stop can still fail its exit code for that.
    ///
    /// The snapshot is best-effort, for the reason it always was: it is a
    /// derived view with a periodic writer behind it, and the log is the source
    /// of truth. Its failure is reported and does not stop the flush.
    pub fn shutdown(&self) -> Result<(), ShutdownError> {
        self.reconcile_stop.store(true, Ordering::Relaxed);
        self.snapshot_stop.store(true, Ordering::Relaxed);
        self.schedule_stop.store(true, Ordering::Relaxed);
        if let Err(error) = self.snapshot() {
            eprintln!("loom-server: writing the domain snapshot on shutdown failed: {error}");
        }
        self.relay.close();
        self.pump.stop();
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

        // The default backend stays in-process; with a data directory the log
        // is materialised on disk as well.
        assert!(dir
            .path()
            .join(format!("shard-{}.log", scope.shard()))
            .exists());

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
            snapshot_interval: Duration::ZERO,
            ..AppConfig::default()
        }
    }

    /// Every state change a thread's log ended with, oldest first.
    fn thread_status_changes(state: &AppState, thread: &ThreadId) -> Vec<String> {
        let frames = state
            .relay
            .replay_scope(&Scope::Thread(thread.to_string()), 100)
            .unwrap();
        let mut changes = Vec::new();
        for frame in &frames {
            let Ok(message) =
                serde_json::from_slice::<crate::protocol::WorkerServerMessage>(&frame.payload)
            else {
                continue;
            };
            let crate::protocol::WorkerServerMessage::Event { payload, .. } = message else {
                continue;
            };
            let Ok(event) = serde_json::from_str::<DomainEvent>(&payload) else {
                continue;
            };
            if let DomainEvent::ThreadStatusChanged { to, .. } = event {
                changes.push(to.to_string());
            }
        }
        changes
    }

    #[tokio::test]
    async fn the_entity_view_survives_a_restart() {
        use loom_domain::EnvironmentKind;

        let dir = TempDir::new().unwrap();
        let (project_id, host_id, environment_id, thread_id);
        {
            let state = AppState::build(durable_config(&dir)).unwrap();
            project_id = state.registry.personal_project_id();
            let (host, _) = state
                .registry
                .enroll_host(None, "laptop".into(), now_ms())
                .unwrap();
            host_id = host.id.clone();
            let (environment, _) = state
                .registry
                .create_environment(
                    Some(state.registry.personal_project_id()),
                    host_id.clone(),
                    EnvironmentKind::Unmanaged,
                    Some("/srv/loom".into()),
                    now_ms(),
                )
                .unwrap();
            environment_id = environment.id.clone();
            let (thread, _) = state
                .registry
                .create_thread(
                    Some(state.registry.personal_project_id()),
                    Some("persisted".into()),
                    Some(environment_id.clone()),
                    now_ms(),
                )
                .unwrap();
            thread_id = thread.id.clone();
            state.shutdown().unwrap();
        }

        let state = AppState::build(durable_config(&dir)).unwrap();
        // The personal project keeps its identity: it is never an event, so
        // only the snapshot can carry it.
        assert_eq!(state.registry.personal_project_id(), project_id);
        assert_eq!(state.registry.host(&host_id).unwrap().name, "laptop");
        let environment = state.registry.environment(&environment_id).unwrap();
        assert_eq!(environment.path.as_deref(), Some("/srv/loom"));
        let thread = state.registry.thread(&thread_id).unwrap();
        assert_eq!(thread.title.as_deref(), Some("persisted"));
        assert_eq!(thread.status, ThreadStatus::Idle);
        assert_eq!(thread.environment_id.as_ref(), Some(&environment_id));
        state.shutdown().unwrap();
    }

    #[tokio::test]
    async fn a_run_in_flight_at_shutdown_is_failed_on_restart() {
        use loom_domain::{EnvironmentKind, MessageRole};

        let dir = TempDir::new().unwrap();
        let thread_id;
        {
            let state = AppState::build(durable_config(&dir)).unwrap();
            let (host, _) = state
                .registry
                .enroll_host(None, "laptop".into(), now_ms())
                .unwrap();
            let (environment, _) = state
                .registry
                .create_environment(
                    Some(state.registry.personal_project_id()),
                    host.id.clone(),
                    EnvironmentKind::Unmanaged,
                    Some("/srv/loom".into()),
                    now_ms(),
                )
                .unwrap();
            let (thread, _) = state
                .registry
                .create_thread(
                    Some(state.registry.personal_project_id()),
                    Some("t".into()),
                    Some(environment.id),
                    now_ms(),
                )
                .unwrap();
            thread_id = thread.id.clone();
            state
                .registry
                .post_message(&thread_id, MessageRole::User, "hi".into(), now_ms())
                .unwrap();
            let thread = state.registry.thread(&thread_id).unwrap();
            assert!(matches!(
                state.dispatch_thread(&thread, "hi"),
                crate::runs::DispatchOutcome::Dispatched(_)
            ));
            assert_eq!(
                state.registry.thread(&thread_id).unwrap().status,
                ThreadStatus::Working
            );
            state.shutdown().unwrap();
        }

        let state = AppState::build(durable_config(&dir)).unwrap();
        // The run cannot be proven alive, so it is failed and the thread leaves
        // `working`; the recovered run is already terminal.
        assert_eq!(
            state.registry.thread(&thread_id).unwrap().status,
            ThreadStatus::Error
        );
        assert!(state.runs.is_empty());
        // State and log agree: the last status change the log holds is the one
        // recovery published, not the stale `working` from before the restart.
        assert_eq!(
            thread_status_changes(&state, &thread_id).last().unwrap(),
            "error"
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
            vec!["turn/started", "provider/error", "turn/completed"]
        );
        let run_id = run_events[0].run_id.clone();
        assert!(run_events.iter().all(|run| {
            run.run_id == run_id && run.event.scope.turn_id() == Some(run_id.to_string()).as_deref()
        }));
        assert_eq!(
            run_events[1].event.body,
            loom_domain::ProviderEvent::ProviderError {
                provider_thread_id: loom_domain::RunEvent::synthetic_provider_thread_id(&run_id),
                message: "server restarted while the run was in flight".into(),
                detail: None,
                error_info: None,
                will_retry: Some(false),
            }
        );
        assert_eq!(
            run_events[2].terminal_error(),
            Some("server restarted while the run was in flight")
        );
        state.shutdown().unwrap();
    }

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

            // Simulate the crash window after the run path appended its start
            // and terminal events but before it removed the registry record or
            // published the thread's final status.
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

    #[tokio::test]
    async fn a_terminal_in_the_log_is_recovered_without_a_run_snapshot() {
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
                    Some("terminal without snapshot".into()),
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
            state.shutdown().unwrap();
        }

        std::fs::remove_file(persistence::snapshot_path(dir.path())).unwrap();
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
            vec!["turn/started", "turn/completed"]
        );
        assert!(run_events.iter().all(|run| run.run_id == run_id));
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
                snapshot_interval: Duration::ZERO,
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

        // Drop the snapshot: the retained log is the only surviving source.
        std::fs::remove_file(persistence::snapshot_path(dir.path())).unwrap();

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

    #[tokio::test]
    async fn a_corrupt_snapshot_falls_back_to_the_log() {
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
        // A torn or bit-rotted snapshot must not stop the server; the log
        // still holds the creation event.
        let path = persistence::snapshot_path(dir.path());
        let mut bytes = std::fs::read(&path).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xff;
        std::fs::write(&path, &bytes).unwrap();

        let state = AppState::build(durable_config(&dir)).unwrap();
        assert!(state.registry.thread(&thread_id).is_some());
        state.shutdown().unwrap();
    }

    #[tokio::test]
    async fn a_snapshot_is_not_written_without_a_data_directory() {
        let state = AppState::build(AppConfig::default()).unwrap();
        assert!(state.snapshot().is_ok());
        state.shutdown().unwrap();
    }
}
