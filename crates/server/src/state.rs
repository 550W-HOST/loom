//! Shared server state.
//!
//! [`AppState`] is deliberately three things and no more: the log, a handle to
//! the fan-out actor, and the readers connecting them. Handlers get this and
//! nothing else — no database handle, no connection registry. A route can
//! publish an event but cannot decide who receives it, which is the property
//! that keeps routing changes out of handlers.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use loom_domain::{
    DomainEvent, DomainScope, HostId, ProjectId, RunEvent, RunId, ThreadId, ThreadStatus,
    ThreadTrigger,
};
use loom_provider_protocol::ProviderSpec;
use loom_relay::retention::Retention;
use loom_relay::{now_ms, Relay, Result as RelayResult};

use crate::domain_state::DomainRegistry;
use crate::hub_actor::HubHandle;
use crate::persistence::{self, DomainSnapshot, SNAPSHOT_VERSION};
use crate::pump::{Pump, PumpConfig};
use crate::runs::{RunRecord, RunRegistry};
use crate::ui::Ui;

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
    /// A shared relay log in Redis Streams.
    ///
    /// `None` (the default) is the single-machine shape. `Some` makes the log
    /// the source of truth for every node pointed at the same Redis and key
    /// prefix, so a server restart or upgrade reattaches to the same window
    /// instead of losing it. Mutually exclusive with [`AppConfig::backend_path`].
    pub backend_redis: Option<loom_relay::backend::redis::RedisConfig>,
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
    /// local daemon is assumed, so primary-host resolution never gets stranded
    /// on an absent local machine. Set `LOOM_LOCAL_HOST_ID` (or this field) on
    /// a single-machine deployment to prefer that machine while its daemon is
    /// attached.
    pub local_host_id: Option<HostId>,
    /// How long a dispatched run may stay in flight before the server reaps it.
    ///
    /// This is the backstop for a daemon that is connected but wedged. The
    /// execution plane enforces its own provider timeout; this one exists so a
    /// silent daemon cannot leave a thread `working` forever.
    pub run_timeout: Duration,
    /// How long a host may go without a heartbeat before it is considered gone
    /// and its in-flight runs are failed.
    ///
    /// Must comfortably exceed the daemon's heartbeat interval; the default is
    /// four times the daemon default.
    pub host_stale_after: Duration,
    /// How often the server runs the timeout/staleness sweep. `Duration::ZERO`
    /// disables the background sweep, which is what unit tests want when they
    /// drive reconciliation explicitly.
    pub reconcile_interval: Duration,
    /// How often a domain snapshot is written to disk.
    ///
    /// Only meaningful when [`AppConfig::backend_path`] names a data
    /// directory: the in-process default keeps no domain state, and the Redis
    /// log is shared across nodes, so neither has a local snapshot to write.
    /// `Duration::ZERO` disables the periodic writer; a snapshot is still
    /// written once, on a clean shutdown.
    pub snapshot_interval: Duration,
    /// The provider the control plane asks execution machines to run.
    pub provider_spec: ProviderSpec,
    /// A built UI bundle to serve. `None` serves the embedded reference
    /// client, so the server always has a UI with zero configuration.
    pub ui_dir: Option<PathBuf>,
    /// A frontend dev server to reverse-proxy unmatched requests to. Mutually
    /// exclusive with [`AppConfig::ui_dir`].
    pub ui_proxy: Option<String>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            node_id: "loom-node".into(),
            backend_max_len: 2_000,
            backend_path: None,
            backend_redis: None,
            retention: Retention::default(),
            hub_queue_capacity: 1_024,
            pump: PumpConfig::default(),
            local_host_id: None,
            run_timeout: Duration::from_secs(30 * 60),
            host_stale_after: Duration::from_secs(60),
            reconcile_interval: Duration::from_secs(5),
            snapshot_interval: Duration::from_secs(30),
            provider_spec: ProviderSpec::pi(),
            ui_dir: None,
            ui_proxy: None,
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

impl From<loom_relay::RelayError> for BuildStateError {
    fn from(error: loom_relay::RelayError) -> Self {
        Self {
            message: error.to_string(),
        }
    }
}

/// Everything a handler needs.
#[derive(Clone)]
pub struct AppState {
    /// The relay log.
    pub relay: Relay,
    /// Handle to the per-node fan-out actor.
    pub hub: HubHandle,
    /// The fixed shard readers.
    pub pump: Arc<Pump>,
    /// In-memory domain entities for the command API.
    pub registry: Arc<DomainRegistry>,
    /// Provider runs that have been dispatched and not yet terminated.
    pub runs: Arc<RunRegistry>,
    /// The static UI source the fallback route serves.
    pub ui: Ui,
    local_host_id: Option<HostId>,
    run_timeout_ms: u64,
    host_stale_after_ms: u64,
    provider_spec: ProviderSpec,
    reconcile_stop: Arc<AtomicBool>,
    snapshot_stop: Arc<AtomicBool>,
    snapshot_root: Option<PathBuf>,
    started_at: Instant,
    started_at_ms: u64,
}

impl AppState {
    /// Wires the relay, hub actor and readers together.
    pub fn build(config: AppConfig) -> Result<Self, BuildStateError> {
        if config.backend_redis.is_some() && config.backend_path.is_some() {
            return Err(BuildStateError {
                message: "LOOM_REDIS_URL (shared) and LOOM_DATA_DIR (local disk) are both set; \
                          choose one backend"
                    .into(),
            });
        }

        let ui = Ui::from_config(config.ui_dir.clone(), config.ui_proxy.clone())
            .map_err(|message| BuildStateError { message })?;

        let backend: loom_relay::SharedBackend = match (&config.backend_redis, &config.backend_path)
        {
            // Shared backend: every node attaches to the same window, so a
            // restart does not drop what a connected daemon already had.
            (Some(redis), None) => Arc::new(loom_relay::backend::redis::RedisBackend::open(
                redis.clone(),
                config.backend_max_len,
            )?),
            // Durable backend: replay survives a restart on this machine.
            (None, Some(path)) => Arc::new(loom_relay::backend::disk::DiskBackend::open(
                path,
                config.backend_max_len,
            )?),
            // Default: in-process, zero external service.
            (None, None) => Arc::new(loom_relay::backend::memory::MemoryBackend::new(
                config.backend_max_len,
            )),
            (Some(_), Some(_)) => unreachable!("guarded above"),
        };
        let relay = Relay::new(backend, config.retention, config.node_id.clone())?;
        let (hub, _actor) = HubHandle::spawn(config.hub_queue_capacity);
        let pump = Arc::new(Pump::spawn(relay.clone(), hub.clone(), config.pump));
        let started_at_ms = now_ms();

        // Domain-state persistence rides on the durable backend: it is the
        // deployment where a restart has a log to recover from. The Redis and
        // in-process backends leave the entity view ephemeral (see
        // `docs/domain-persistence.md`).
        let snapshot_root = config.backend_path.clone();

        let state = Self {
            relay,
            hub,
            pump,
            registry: Arc::new(DomainRegistry::new(started_at_ms)),
            runs: Arc::new(RunRegistry::new()),
            ui,
            local_host_id: config.local_host_id,
            run_timeout_ms: config.run_timeout.as_millis().min(u128::from(u64::MAX)) as u64,
            host_stale_after_ms: config
                .host_stale_after
                .as_millis()
                .min(u128::from(u64::MAX)) as u64,
            provider_spec: config.provider_spec,
            reconcile_stop: Arc::new(AtomicBool::new(false)),
            snapshot_stop: Arc::new(AtomicBool::new(false)),
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
            }
        });
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

    /// The provider the control plane asks execution machines to run.
    pub(crate) fn provider_spec(&self) -> &ProviderSpec {
        &self.provider_spec
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
        let envelope = self
            .relay
            .publish_with(scope, move |event_id, created_at_ms| {
                crate::protocol::build_event_frame(&frame_scope, &payload, event_id, created_at_ms)
            })?;
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
        let payload = serde_json::to_vec(event).expect("a DomainEvent always serializes to JSON");
        self.publish(relay_scope(&event.scope()), payload)
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
    /// run is left without a terminal event. The daemon's later report is an
    /// idempotent no-op (`ReportOutcome::Unknown`). Returns how many runs were
    /// failed.
    fn fail_in_flight_runs(&self, records: Vec<RunRecord>, now: u64) -> usize {
        let mut failed = 0;
        for record in &records {
            if self.fail_recovered_run(&record.run_id, &record.thread_id, &record.project_id, now) {
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
            let run_id = thread.active_run_id.clone().unwrap_or_else(RunId::mint);
            if self.fail_recovered_run(&run_id, &thread.id, &thread.project_id, now) {
                failed += 1;
            }
        }
        failed
    }

    /// Fails one recovered run, returning whether the thread was moved.
    ///
    /// Idempotent: a thread already out of `working` is left alone, so the
    /// snapshot's run list and the thread sweep cannot double-report.
    fn fail_recovered_run(
        &self,
        run_id: &RunId,
        thread_id: &ThreadId,
        project_id: &ProjectId,
        now: u64,
    ) -> bool {
        let Some(thread) = self.registry.thread(thread_id) else {
            return false;
        };
        if !matches!(thread.status, ThreadStatus::Working | ThreadStatus::Waiting) {
            return false;
        }
        let event = DomainEvent::ThreadRunEvent {
            thread_id: thread_id.clone(),
            project_id: project_id.clone(),
            run_id: run_id.clone(),
            at_ms: now,
            event: RunEvent::Finished {
                outcome: loom_domain::RunOutcome::Failed,
                error: Some("server restarted while the run was in flight".into()),
            },
        };
        let _ = self.publish_domain_event(&event);
        let _ = self.registry.clear_thread_run(thread_id, now);
        if let Ok(Some(change)) =
            self.registry
                .transition_thread(thread_id, ThreadTrigger::RunFailed, now)
        {
            let _ = self.publish_domain_event(&change);
        }
        true
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
        let watermark = self
            .relay
            .high_watermark()
            .map_err(|error| persistence::SnapshotError::Io(error.to_string()))?;
        let snapshot = DomainSnapshot {
            version: SNAPSHOT_VERSION,
            watermark,
            registry: self.registry.export(),
            runs: self.runs.all(),
        };
        persistence::write_snapshot(root, &snapshot)
    }

    /// Stops the readers and the reconciler, and writes a final snapshot.
    ///
    /// `&self` because every task is shared. The snapshot is best-effort: a
    /// failure is reported and does not prevent the process from exiting, and
    /// the periodic writer means at most one interval of state is at risk.
    pub fn shutdown(&self) {
        self.reconcile_stop.store(true, Ordering::Relaxed);
        self.snapshot_stop.store(true, Ordering::Relaxed);
        if let Err(error) = self.snapshot() {
            eprintln!("loom-server: writing the domain snapshot on shutdown failed: {error}");
        }
        self.pump.stop();
    }
}

/// Reads the domain event out of a stored relay frame, if it holds one.
///
/// A frame only counts when its payload parses as a [`DomainEvent`] whose own
/// scope is the frame's scope. Run dispatches and any raw producer payloads
/// share the log, so "is it JSON object with a `type` tag" is not enough.
fn domain_event_from_envelope(envelope: &loom_relay::Envelope) -> Option<DomainEvent> {
    let message: crate::protocol::ServerMessage = serde_json::from_slice(&envelope.payload).ok()?;
    let crate::protocol::ServerMessage::Event { payload, .. } = message else {
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

#[cfg(test)]
mod tests {
    use super::*;
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

        state.shutdown();
    }

    #[tokio::test]
    async fn a_published_event_is_replayable() {
        let state = AppState::build(AppConfig::default()).unwrap();
        let scope = Scope::Thread("thr_1".into());
        let envelope = state.publish(scope.clone(), "{\"n\":1}").unwrap();

        let replayed = state.relay.replay_scope(&scope, 10).unwrap();
        assert_eq!(replayed.len(), 1);
        assert_eq!(replayed[0].event_id, envelope.event_id);

        state.shutdown();
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

        state.shutdown();
    }

    #[tokio::test]
    async fn refuses_two_durable_backends_at_once() {
        let dir = TempDir::new().unwrap();
        let config = AppConfig {
            backend_path: Some(dir.path().to_path_buf()),
            backend_redis: Some(loom_relay::backend::redis::RedisConfig::new(
                "127.0.0.1",
                6_379,
            )),
            ..AppConfig::default()
        };
        let error = AppState::build(config).unwrap_err();
        assert!(error.to_string().contains("choose one backend"));
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
                serde_json::from_slice::<crate::protocol::ServerMessage>(&frame.payload)
            else {
                continue;
            };
            let crate::protocol::ServerMessage::Event { payload, .. } = message else {
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
                    None,
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
                    None,
                    Some("persisted".into()),
                    Some(environment_id.clone()),
                    now_ms(),
                )
                .unwrap();
            thread_id = thread.id.clone();
            state.shutdown();
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
        state.shutdown();
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
                    None,
                    host.id.clone(),
                    EnvironmentKind::Unmanaged,
                    Some("/srv/loom".into()),
                    now_ms(),
                )
                .unwrap();
            let (thread, _) = state
                .registry
                .create_thread(None, Some("t".into()), Some(environment.id), now_ms())
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
            state.shutdown();
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
        state.shutdown();
    }

    #[tokio::test]
    async fn a_log_without_a_snapshot_rebuilds_without_panicking() {
        let dir = TempDir::new().unwrap();
        let thread_id;
        {
            let state = AppState::build(durable_config(&dir)).unwrap();
            let (thread, created) = state
                .registry
                .create_thread(None, Some("t".into()), None, now_ms())
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
            state.shutdown();
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
        state.shutdown();
    }

    #[tokio::test]
    async fn a_corrupt_snapshot_falls_back_to_the_log() {
        let dir = TempDir::new().unwrap();
        let thread_id;
        {
            let state = AppState::build(durable_config(&dir)).unwrap();
            let (thread, created) = state
                .registry
                .create_thread(None, Some("t".into()), None, now_ms())
                .unwrap();
            thread_id = thread.id.clone();
            state.publish_domain_event(&created).unwrap();
            state.shutdown();
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
        state.shutdown();
    }

    #[tokio::test]
    async fn a_snapshot_is_not_written_without_a_data_directory() {
        let state = AppState::build(AppConfig::default()).unwrap();
        assert!(state.snapshot().is_ok());
        state.shutdown();
    }
}
