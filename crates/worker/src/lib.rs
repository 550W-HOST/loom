//! The loom worker.
//!
//! This is the **execution plane as an independent process**. It connects
//! *outbound* to a server URL, enrolls as a host, keeps that host alive with
//! heartbeats, follows its own `host:{id}` room, and runs the provider CLIs
//! dispatched to it. It never needs the server to share its machine, process
//! tree, or cgroup, and the server never starts or supervises it.
//!
//! # Why it exists before the Node execution plane
//!
//! bb's `apps/host-daemon` is the real execution plane and it is ~45k lines,
//! but the *boundary* it sits behind is small: enroll, heartbeat, receive
//! dispatch through a scope, report events back. This crate implements exactly
//! that boundary, with an ACP client and the embedded `pi-acp` adapter for Pi.
//! When the Node host daemon lands it replaces the execution implementation, not the
//! contract.
//!
//! # Lifecycle
//!
//! ```text
//!   connect ──▶ hello ──▶ enroll_host ──▶ host_enrolled ──▶ subscribe host:{id}
//!                                                   │              │
//!                                                   │              ▼
//!                                                   │        replay since cursor
//!                                                   │        (missed dispatches)
//!                        heartbeat ─────────────────┤
//!                                                   │
//!                        dispatch ──▶ spawn provider ──▶ reports ──▶ server
//!                                                   │
//!                        host_disconnect / close ────┘ (on shutdown)
//! ```
//!
//! Two properties matter and both are enforced in code, not convention:
//!
//! * **Dispatch arrives through the relay.** The worker subscribes to
//!   `host:{id}` and parses [`RunDispatch`] out of relayed event payloads. It
//!   replays from its cursor on reconnect, so a dispatch published while it was
//!   disconnected is delivered late rather than lost.
//! * **A run always ends.** Every provider process ends in exactly one
//!   `finished` report — see [`provider`] — and the server separately reaps a
//!   run whose deadline passes, so a worker that dies mid-run cannot leave the
//!   thread `working`.
//!
//! [`RunDispatch`]: loom_provider_protocol::RunDispatch

pub mod acp;
pub mod cli;
pub mod discovery;
pub mod failure_text;
pub mod host_files;
pub mod provider;
pub mod run;
pub mod scripts;
pub mod session;
pub mod steer;
pub mod terminal;
pub mod update;
pub mod workspace;
pub mod worktree;

use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use loom_domain::{HostId, RunId};
use loom_provider_protocol::{
    EnvironmentDeprovision, EnvironmentDeprovisionOutcome, EnvironmentDeprovisionReport,
    EnvironmentProvision, EnvironmentProvisionOutcome, EnvironmentProvisionReport,
    EnvironmentProvisionWorkspace, HistoryPart, HistoryReport, HostFileRequest, HostRpcOperation,
    HostRpcReport, HostRpcRequest, InteractionRequest, InteractionResolutionFrame,
    ProviderCatalogReport, ProviderCommandsReport, ProviderLaunch, ProviderSpec, RunDispatch,
    RunSteer,
};
use loom_relay::dedup::SeenSet;
use loom_relay::{EventId, Scope};
use loom_server::protocol::{
    WorkerClientMessage as ClientCommand, WorkerServerMessage as ServerMessage,
};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

use crate::provider::ProviderRun;

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// How often a connected worker reports liveness by default.
pub const DEFAULT_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);

/// How long a provider run may take before the worker kills it, by default.
///
/// This bounds the run's **silence**, not the turn: every event the run reports
/// re-arms it, and an item that started and has not completed holds it off, so
/// a long build, a long download, a forked child agent or a long streamed
/// answer is never mistaken for a stuck agent. See
/// [`DEFAULT_RUN_CEILING`] for the bound that a still-talking run eventually
/// hits.
pub const DEFAULT_RUN_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// How long a provider run may take *in total*, however active it is, before
/// the worker kills it.
///
/// This is the last-resort bound rather than the normal one. A run that keeps
/// reporting is not stuck, so [`DEFAULT_RUN_TIMEOUT`] cannot end it; only an
/// agent that wedged with a tool call still open reaches this ceiling. It is
/// deliberately much longer than any real turn so that it never decides the
/// fate of healthy work.
pub const DEFAULT_RUN_CEILING: Duration = Duration::from_secs(6 * 60 * 60);

/// How long an accepted turn may stay *silent*, by default, before the
/// embedded `pi-acp` gives up on it through its own settle fallback.
///
/// The adapter's own copy of the rule [`DEFAULT_RUN_TIMEOUT`] applies from the
/// worker's side: it bounds silence, not the turn, so a long tool or a long
/// streamed answer is never mistaken for a stuck agent. The two differ in who
/// owns the timer — the adapter fails a prompt that never got going, the worker
/// fails a run that stopped reporting — and in what they can see: the adapter
/// holds the bound off for a tool it knows is open, the worker for an item it
/// has seen start without a completion. loom states the value instead of
/// leaving it to `pi_acp::Config::default()` because the embedded path never
/// reads `PI_ACP_SETTLE_TIMEOUT_SECS`, so this is where the value it runs with
/// is decided (`--settle-timeout-ms` overrides it).
pub const DEFAULT_SETTLE_TIMEOUT: Duration =
    Duration::from_secs(pi_acp::config::DEFAULT_SETTLE_TIMEOUT_SECS);

/// How many dispatch ids the worker remembers to suppress redelivery.
pub const DISPATCH_DEDUP_CAPACITY: usize = 512;

/// How many reports may be queued before a provider task waits.
const REPORT_CHANNEL_CAPACITY: usize = 256;

/// How long a permission request waits for a user by default.
///
/// Re-exported from the ACP permission bridge, which is where the reasoning
/// lives; `main.rs` uses it for `--permission-timeout-ms`.
pub const DEFAULT_PERMISSION_TIMEOUT: Duration = crate::acp::permission::DEFAULT_PERMISSION_TIMEOUT;

/// How long the startup catalogue probe waits for the agent to answer.
///
/// The probe opens one throwaway session, so the budget has to cover an agent's
/// cold start. It runs off the socket loop and its failure is only logged: an
/// agent that cannot answer yet must not keep the worker from enrolling.
pub const DEFAULT_CATALOG_PROBE_BUDGET: Duration = Duration::from_secs(30);

/// How long one history load may take before it fails.
///
/// Like the catalogue probe, a load opens a throwaway session, so the budget
/// has to cover an agent's cold start plus a full replay. Exceeding it is
/// reported to the server as a failure rather than answered with whatever
/// arrived, because a partial conversation is indistinguishable from a short
/// one once it is cached.
pub const DEFAULT_HISTORY_LOAD_BUDGET: Duration = Duration::from_secs(60);

/// Where managed environments' workspaces are created by default.
///
/// `$HOME/.loom/workspaces`, or the system temp directory when there is no
/// home. `--workspace-root` overrides it. The worker owns this layout: the
/// control plane only learns the resulting path from the report.
///
/// Deliberately independent of [`default_data_dir`]: a data directory is where
/// a machine's *own* data lives, a workspace root is where work happens, and
/// defaulting one from the other would silently relocate every existing
/// deployment's workspaces the moment `--data-dir` was set.
pub fn default_environment_root() -> PathBuf {
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".loom").join("workspaces");
    }
    std::env::temp_dir().join("loom-workspaces")
}

/// The worker's own data directory on this machine.
///
/// `$HOME/.loom`, or the system temp directory when there is no home.
/// `--data-dir` overrides it. This is the root the worker reports at
/// enrollment and the one thread storage is named from, so the control plane
/// never has to guess where a machine keeps its data.
pub fn default_data_dir() -> PathBuf {
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".loom");
    }
    std::env::temp_dir().join("loom-data")
}

/// A worker could not connect, could not speak the protocol, or was rejected.
#[derive(Debug)]
pub enum WorkerError {
    /// The socket could not be opened or failed mid-conversation.
    WebSocket(String),
    /// The server sent a frame this worker could not use.
    Protocol(String),
    /// The server announced a protocol version this build cannot speak.
    ///
    /// Kept apart from the general [`WorkerError::Protocol`] because it is the
    /// one protocol failure with an automatic remedy: the reconnect loop reads
    /// the version out of it and fetches the matching worker
    /// ([`crate::update`]).
    ProtocolMismatch {
        /// The version the server announced.
        server_protocol_version: u32,
        /// The version this binary speaks.
        local_protocol_version: u32,
    },
}

impl std::fmt::Display for WorkerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WorkerError::WebSocket(message) => write!(f, "websocket: {message}"),
            WorkerError::Protocol(message) => write!(f, "protocol: {message}"),
            WorkerError::ProtocolMismatch {
                server_protocol_version,
                local_protocol_version,
            } => write!(
                f,
                "server speaks protocol version {server_protocol_version}, \
                 this worker speaks {local_protocol_version}; this worker must be updated \
                 (upgrade server and worker together, or let the worker self-update)"
            ),
        }
    }
}

impl WorkerError {
    /// The protocol version the peer announced, for a version refusal.
    ///
    /// This is the one piece of information the reconnect loop needs out of a
    /// failed connection: it is what the update is requested against, so the
    /// binary fetched is for exactly the version the server announced rather
    /// than for a re-read of it that could have moved.
    pub fn mismatched_protocol_version(&self) -> Option<u32> {
        match self {
            WorkerError::ProtocolMismatch {
                server_protocol_version,
                ..
            } => Some(*server_protocol_version),
            _ => None,
        }
    }
}

impl std::error::Error for WorkerError {}

impl From<tokio_tungstenite::tungstenite::Error> for WorkerError {
    fn from(error: tokio_tungstenite::tungstenite::Error) -> Self {
        WorkerError::WebSocket(error.to_string())
    }
}

impl From<serde_json::Error> for WorkerError {
    fn from(error: serde_json::Error) -> Self {
        WorkerError::Protocol(error.to_string())
    }
}

/// Refuses a server whose hello protocol version this worker cannot speak.
///
/// The server and every worker and UI bundle must agree on
/// [`loom_server::PROTOCOL_VERSION`]: it is the wire contract, not a marketing
/// version. A mixed deployment is rejected here, at the first frame, rather
/// than misbehaving mid-run. See `docs/upgrades.md`.
///
/// A newer server is exactly the case [`crate::update`] handles: the reconnect
/// loop reads the version out of the error, asks the same server for the
/// matching binary, installs it and restarts. Keeping the refusal here —
/// before `enroll`, before a single dispatch — is what makes an update safe to
/// perform: no run is in flight on a connection that never enrolled.
pub fn ensure_compatible_protocol(server_protocol_version: u32) -> Result<(), WorkerError> {
    let local = loom_server::PROTOCOL_VERSION;
    if server_protocol_version == local {
        Ok(())
    } else {
        Err(WorkerError::ProtocolMismatch {
            server_protocol_version,
            local_protocol_version: local,
        })
    }
}

/// Everything a worker needs to reach and describe itself.
#[derive(Clone, Debug)]
pub struct WorkerConfig {
    /// The server to connect to: `http://host:port`, `https://…`, or an
    /// explicit `ws(s)://` URL. The worker only ever dials out.
    pub server_url: String,
    /// The machine's display name.
    pub name: String,
    /// The identity to reuse on reconnect. `None` on a first enrollment.
    pub host_id: Option<HostId>,
    /// How often to heartbeat once connected.
    pub heartbeat_interval: Duration,
    /// Override the provider the control plane dispatches. `None` uses the
    /// provider named in the dispatch, which is the normal case; an operator
    /// sets this on a machine whose provider lives at a non-standard path.
    pub provider: Option<ProviderSpec>,
    /// The agents this machine has installed.
    ///
    /// `None` — what the daemon leaves it as — discovers them from this machine
    /// at every connect, so installing an agent and reconnecting is the whole
    /// update path. `Some` is for a caller that has already decided: a test
    /// wants the provider list to be a property of the test rather than of the
    /// machine running it. See [`WorkerConfig::without_discovery`].
    pub discovered: Option<Vec<ProviderSpec>>,
    /// How long one provider run may stay *silent* before it is killed.
    ///
    /// See [`DEFAULT_RUN_TIMEOUT`].
    pub run_timeout: Duration,
    /// How long one provider run may take in total, however active it is.
    ///
    /// See [`DEFAULT_RUN_CEILING`].
    pub run_ceiling: Duration,
    /// How long an accepted turn may stay silent before the embedded `pi-acp`'s
    /// settle fallback gives up on it. See [`DEFAULT_SETTLE_TIMEOUT`].
    pub settle_timeout: Duration,
    /// How long an agent's permission request waits for a user before it is
    /// cancelled. See [`DEFAULT_PERMISSION_TIMEOUT`].
    pub permission_timeout: Duration,
    /// Root under which managed environments' workspaces are created.
    ///
    /// A managed environment's directory is `<environment_root>/<env_id>`. The
    /// worker owns the directory layout and reports the resulting path; the ACP
    /// agent owns its own session storage.
    pub environment_root: PathBuf,
    /// The worker's own data directory, reported at enrollment.
    ///
    /// Thread storage is named from it (`<data_dir>/thread-storage/<thread>`),
    /// so a worker that does not report one leaves those routes answering `501`
    /// rather than reading a path the control plane invented.
    pub data_dir: PathBuf,
    /// The host-scope event id to resume from. `None` replays the retained
    /// window and relies on dispatch dedup.
    pub resume_cursor: Option<EventId>,
    /// An optional one-time enrollment code issued by the server.
    pub join_code: Option<String>,
    /// Maximum frames to request in the reconnect replay.
    pub replay_limit: usize,
    /// How the worker reacts to a server whose protocol does not match.
    ///
    /// [`crate::update::UpdateConfig`] carries "allowed at all", the install
    /// path and the backoff schedule. The reconnect loop is the only party that
    /// uses it, and it is what turns the old hard refusal into "fetch the
    /// matching binary and restart".
    pub update: crate::update::UpdateConfig,
}

impl WorkerConfig {
    /// A configuration with the default heartbeat interval and no provider
    /// override.
    ///
    /// The update configuration defaults to "enabled, install over the running
    /// executable, no persisted state", so a worker started with no flags at
    /// all still follows a newer server. `UpdateConfig::for_current_binary`
    /// reports the one machine-level failure it can have (a running executable
    /// that cannot be resolved); when it does, self-update is turned off loudly
    /// rather than left half-configured.
    pub fn new(server_url: impl Into<String>, name: impl Into<String>) -> Self {
        let update =
            crate::update::UpdateConfig::for_current_binary(true, None).unwrap_or_else(|error| {
                eprintln!("loom-worker: self-update disabled: {error}");
                crate::update::UpdateConfig {
                    enabled: false,
                    install_path: PathBuf::new(),
                    state_dir: None,
                    target: loom_server::TARGET.to_owned(),
                    initial_backoff: crate::update::DEFAULT_INITIAL_BACKOFF,
                    max_backoff: crate::update::DEFAULT_MAX_BACKOFF,
                }
            });
        Self {
            server_url: server_url.into(),
            name: name.into(),
            host_id: None,
            heartbeat_interval: DEFAULT_HEARTBEAT_INTERVAL,
            provider: None,
            discovered: None,
            run_timeout: DEFAULT_RUN_TIMEOUT,
            run_ceiling: DEFAULT_RUN_CEILING,
            settle_timeout: DEFAULT_SETTLE_TIMEOUT,
            permission_timeout: DEFAULT_PERMISSION_TIMEOUT,
            environment_root: default_environment_root(),
            data_dir: default_data_dir(),
            join_code: None,
            resume_cursor: None,
            replay_limit: 500,
            update,
        }
    }

    /// Declares that this worker has no installed agents to discover.
    ///
    /// A test wants the provider list to be a property of the test rather than
    /// of the machine running it, and probing whatever happens to be installed
    /// on a developer's machine makes a suite both slower and less
    /// deterministic. Nothing a daemon does calls this: the real worker leaves
    /// [`WorkerConfig::discovered`] unset and finds its own agents.
    pub fn without_discovery(mut self) -> Self {
        self.discovered = Some(Vec::new());
        self
    }

    /// The internal worker WebSocket endpoint derived from
    /// [`WorkerConfig::server_url`].
    ///
    /// Accepts the URL an operator would paste into a browser and turns it
    /// into the socket path, so "the server is a URL" holds for workers too.
    pub fn websocket_url(&self) -> String {
        let base = self.server_url.trim_end_matches('/');
        let base = if let Some(rest) = base.strip_prefix("https://") {
            format!("wss://{rest}")
        } else if let Some(rest) = base.strip_prefix("http://") {
            format!("ws://{rest}")
        } else if base.starts_with("ws://") || base.starts_with("wss://") {
            base.to_owned()
        } else {
            format!("ws://{base}")
        };
        if base.ends_with("/internal/ws") {
            base
        } else if let Some(prefix) = base.strip_suffix("/ws") {
            format!("{prefix}/internal/ws")
        } else {
            format!("{base}/internal/ws")
        }
    }
}

/// A bounded set of run ids, for suppressing dispatch redelivery.
#[derive(Debug)]
struct RunSeen {
    order: VecDeque<RunId>,
    seen: HashSet<RunId>,
    capacity: usize,
}

impl RunSeen {
    fn new(capacity: usize) -> Self {
        Self {
            order: VecDeque::new(),
            seen: HashSet::new(),
            capacity: capacity.max(1),
        }
    }

    /// Records a run id, returning `true` the first time it is seen.
    fn insert(&mut self, run_id: &RunId) -> bool {
        if !self.seen.insert(run_id.clone()) {
            return false;
        }
        self.order.push_back(run_id.clone());
        while self.order.len() > self.capacity {
            if let Some(evicted) = self.order.pop_front() {
                self.seen.remove(&evicted);
            }
        }
        true
    }

    fn contains(&self, run_id: &RunId) -> bool {
        self.seen.contains(run_id)
    }
}

/// A connected worker.
///
/// Created by [`Worker::connect`], identified by [`Worker::enroll`], kept
/// alive by [`Worker::run`] or [`Worker::heartbeat`].
pub struct Worker {
    socket: Socket,
    config: WorkerConfig,
    /// The agents the server can dispatch, as it announced them at connect.
    ///
    /// The worker probes each one's catalogue at enrollment, so this is what
    /// makes a second agent show real models before any run has happened. An
    /// older server that sends none falls back to the worker's own spec, which
    /// is the single-provider behaviour this replaced.
    providers: Vec<ProviderSpec>,
    /// The agents loom found installed on this machine.
    ///
    /// Discovered once at connect from [`crate::discovery::KNOWN_AGENTS`] and
    /// this process's `PATH`. This — not the server's list — is what the host
    /// reports it can run, because only the machine knows what is installed on
    /// it.
    discovered: Vec<ProviderSpec>,
    host_id: Option<HostId>,
    /// Highest host-scope event id applied, for reconnect replay.
    cursor: Option<EventId>,
    /// Event ids already applied on this connection, so replay and the live
    /// path can overlap without double-processing.
    seen_events: SeenSet,
    /// Dispatch ids already started, so a redelivered dispatch is a no-op.
    seen_runs: RunSeen,
    /// Provider reports waiting to be forwarded to the server.
    reports: mpsc::Receiver<loom_provider_protocol::ProviderReport>,
    reports_tx: mpsc::Sender<loom_provider_protocol::ProviderReport>,
    /// Agent catalogues waiting to be forwarded to the server.
    ///
    /// A catalogue is a fact about the host's agent, not about a run, so it has
    /// its own channel rather than riding a run report. Two producers feed it:
    /// the startup probe, once per enrollment, and every ACP session, which
    /// reads the catalogue out of the config options it already holds.
    catalog_reports: mpsc::Receiver<ProviderCatalogReport>,
    catalog_reports_tx: mpsc::Sender<ProviderCatalogReport>,
    /// Command lists advertised by ACP sessions, waiting to be forwarded.
    ///
    /// A command list is a fact about a workspace and an agent, not about a
    /// run, so it has its own channel rather than riding a run report.
    command_reports: mpsc::Receiver<ProviderCommandsReport>,
    command_reports_tx: mpsc::Sender<ProviderCommandsReport>,
    /// The verified agent list waiting to be forwarded to the server.
    ///
    /// One per probe that answered, each the complete set verified so far, so a
    /// candidate that never finishes its handshake delays its own appearance and
    /// nothing else's. The last one is the full list.
    provider_lists: mpsc::Receiver<Vec<ProviderSpec>>,
    provider_lists_tx: mpsc::Sender<Vec<ProviderSpec>>,
    /// Permission requests raised by providers, waiting to be forwarded.
    ///
    /// The socket loop is the only thing that may write to the server socket,
    /// so a provider's question arrives here and the loop forwards it, exactly
    /// as a run report does.
    interactions: mpsc::Receiver<InteractionRequest>,
    interactions_tx: mpsc::Sender<InteractionRequest>,
    /// The permission requests currently held open, so an answer arriving on
    /// the socket can be handed to the provider task waiting for it.
    permissions: crate::acp::permission::PermissionRegistry,
    /// The live ACP turns a steer can join, keyed by run id.
    ///
    /// The socket loop receives a [`RunSteer`] and hands it here; the run's own
    /// conversation task registered the receiving half when its session was
    /// established. A steer for a run that is not present has no turn to join
    /// and is dropped — see [`crate::steer`].
    steers: crate::steer::SteerRegistry,
    /// Environment-provisioning reports waiting to be forwarded to the server.
    env_reports: mpsc::Receiver<EnvironmentProvisionReport>,
    env_reports_tx: mpsc::Sender<EnvironmentProvisionReport>,
    /// Environment-teardown reports waiting to be forwarded to the server.
    env_deprovision_reports: mpsc::Receiver<EnvironmentDeprovisionReport>,
    env_deprovision_reports_tx: mpsc::Sender<EnvironmentDeprovisionReport>,
    /// Host file answers waiting to be forwarded to the server.
    ///
    /// A read or listing runs off the socket loop (see
    /// [`crate::host_files`]) and its answer comes back here for the loop to
    /// forward, because the loop is the only thing that may write to the
    /// server socket.
    host_file_reports: mpsc::Receiver<loom_provider_protocol::HostFileReport>,
    host_file_reports_tx: mpsc::Sender<loom_provider_protocol::HostFileReport>,
    /// Workspace and git answers waiting to be forwarded to the server.
    host_rpc_reports: mpsc::Receiver<HostRpcReport>,
    host_rpc_reports_tx: mpsc::Sender<HostRpcReport>,
    /// Streamed history frames waiting to be forwarded to the server.
    ///
    /// One load produces several frames, so this is not folded into
    /// `host_rpc_reports`: that channel's contract is one answer per request.
    history_reports: mpsc::Receiver<loom_provider_protocol::HistoryReport>,
    history_reports_tx: mpsc::Sender<loom_provider_protocol::HistoryReport>,
    /// Terminal answers waiting to be forwarded to the server.
    terminal_reports: mpsc::Receiver<loom_provider_protocol::TerminalReport>,
    terminal_reports_tx: mpsc::Sender<loom_provider_protocol::TerminalReport>,
    /// Automation-script results waiting to be forwarded to the server.
    script_reports: mpsc::Receiver<loom_provider_protocol::ScriptRunReport>,
    script_reports_tx: mpsc::Sender<loom_provider_protocol::ScriptRunReport>,
    /// The PTY sessions this worker holds.
    terminal_sessions: crate::terminal::TerminalRegistry,
    /// The automation scripts this worker is running.
    ///
    /// Held here rather than created per dispatch because a cancel has to
    /// reach a running process: the runner is what maps a run id to the task
    /// that owns the child.
    scripts: Option<crate::scripts::ScriptRunner>,
    /// Runs with a provider task in flight, keyed by run id.
    running: HashSet<RunId>,
}

impl Worker {
    /// Opens the internal socket and consumes the server's hello frame.
    pub async fn connect(config: WorkerConfig) -> Result<Self, WorkerError> {
        let url = config.websocket_url();
        let (mut socket, _) = connect_async(&url).await?;
        match next_message(&mut socket).await? {
            ServerMessage::Hello {
                protocol_version,
                providers,
            } => {
                // Refuse a peer this build cannot speak to, before enrolling.
                // A mismatch after enrollment would corrupt dispatch/runs.
                ensure_compatible_protocol(protocol_version)?;
                let (reports_tx, reports) = mpsc::channel(REPORT_CHANNEL_CAPACITY);
                let (catalog_reports_tx, catalog_reports) = mpsc::channel(REPORT_CHANNEL_CAPACITY);
                let (command_reports_tx, command_reports) = mpsc::channel(REPORT_CHANNEL_CAPACITY);
                let (provider_lists_tx, provider_lists) = mpsc::channel(1);
                let (interactions_tx, interactions) = mpsc::channel(REPORT_CHANNEL_CAPACITY);
                let (env_reports_tx, env_reports) = mpsc::channel(REPORT_CHANNEL_CAPACITY);
                let (env_deprovision_reports_tx, env_deprovision_reports) =
                    mpsc::channel(REPORT_CHANNEL_CAPACITY);
                let (host_file_reports_tx, host_file_reports) =
                    mpsc::channel(REPORT_CHANNEL_CAPACITY);
                let (host_rpc_reports_tx, host_rpc_reports) =
                    mpsc::channel(REPORT_CHANNEL_CAPACITY);
                let (history_reports_tx, history_reports) = mpsc::channel(REPORT_CHANNEL_CAPACITY);
                let (terminal_reports_tx, terminal_reports) =
                    mpsc::channel(REPORT_CHANNEL_CAPACITY);
                let (script_reports_tx, script_reports) = mpsc::channel(REPORT_CHANNEL_CAPACITY);
                // Discovery is per connection, not per process: an agent
                // installed while the worker is running is offered as soon as
                // it reconnects, without the daemon having to restart.
                let discovered = config
                    .discovered
                    .clone()
                    .unwrap_or_else(crate::discovery::candidates);
                Ok(Self {
                    socket,
                    cursor: config.resume_cursor,
                    config,
                    providers,
                    discovered,
                    host_id: None,
                    seen_events: SeenSet::new(DISPATCH_DEDUP_CAPACITY),
                    seen_runs: RunSeen::new(DISPATCH_DEDUP_CAPACITY),
                    reports,
                    reports_tx,
                    catalog_reports,
                    catalog_reports_tx,
                    command_reports,
                    command_reports_tx,
                    provider_lists,
                    provider_lists_tx,
                    interactions,
                    interactions_tx,
                    permissions: crate::acp::permission::PermissionRegistry::new(),
                    steers: crate::steer::SteerRegistry::new(),
                    env_reports,
                    env_reports_tx,
                    env_deprovision_reports,
                    env_deprovision_reports_tx,
                    host_file_reports,
                    host_file_reports_tx,
                    host_rpc_reports,
                    host_rpc_reports_tx,
                    history_reports,
                    history_reports_tx,
                    terminal_reports,
                    terminal_reports_tx,
                    script_reports,
                    script_reports_tx,
                    terminal_sessions: crate::terminal::TerminalRegistry::new(),
                    // Built at enrollment: a script runs in a directory named
                    // after the automation under the worker's data directory,
                    // which is a fact about the enrolled host.
                    scripts: None,
                    running: HashSet::new(),
                })
            }
            other => Err(WorkerError::Protocol(format!(
                "expected hello, got {other:?}"
            ))),
        }
    }

    /// Enrolls as a host, follows its `host:{id}` room, and replays anything it
    /// missed while disconnected.
    ///
    /// After this returns, the worker is receiving dispatches: live ones from
    /// the room and anything published while it was away, replayed from the
    /// relay's retained window.
    pub async fn enroll(&mut self) -> Result<HostId, WorkerError> {
        self.send(&ClientCommand::EnrollHost {
            host_id: self.config.host_id.clone(),
            name: self.config.name.clone(),
            // The machine says where its data lives; the control plane never
            // guesses it. An empty value is normalised away rather than sent.
            data_dir: Some(self.config.data_dir.to_string_lossy().into_owned())
                .filter(|dir| !dir.trim().is_empty()),
            join_code: self.config.join_code.clone(),
        })
        .await?;

        let host_id = loop {
            match next_message(&mut self.socket).await? {
                ServerMessage::HostEnrolled { host, .. } => break host.id,
                ServerMessage::Error { message } => {
                    return Err(WorkerError::Protocol(message));
                }
                _ => continue,
            }
        };
        self.host_id = Some(host_id.clone());
        // The script runner needs the enrolled host and the data directory a
        // script's workspace is named after, and both are known now.
        self.scripts = Some(crate::scripts::ScriptRunner::new(
            host_id.clone(),
            self.config.data_dir.clone(),
            self.config.server_url.clone(),
            self.script_reports_tx.clone(),
        ));

        // Follow the room first, then replay: a live dispatch that arrives in
        // between is queued on the socket and also present in the replay
        // window, and the dedup set drops the overlap.
        self.subscribe(Scope::Host(host_id.to_string())).await?;
        self.replay_host_scope(&host_id).await?;
        // The host id is known now, which is all the catalogue report needs.
        // This runs in the background: a slow or missing agent delays the
        // catalogue, never enrollment.
        self.probe_catalog(&host_id);
        Ok(host_id)
    }

    /// Asks each candidate agent what it can run, and reports the ones that
    /// answered.
    ///
    /// The probe validates installed candidates: `initialize` is always
    /// required, while agents that need a session to describe their catalogue
    /// also complete `session/new`. The catalogue and verified list are products
    /// of the same run: the catalogue fills the picker, and the list decides
    /// what the control plane may dispatch.
    ///
    /// Every probe runs in its own task, and the verified list is reported as
    /// each one settles: one slow candidate must not hold back the agents that
    /// already answered, or a machine with one broken agent would look like a
    /// machine with none.
    fn probe_catalog(&self, host_id: &HostId) {
        // Session-based probes use the worker's own directory: they are not
        // about a project, they only need a workspace the agent accepts. An
        // initialize-only probe still inherits this cwd without opening a
        // persistent session.
        let cwd = std::env::current_dir()
            .map(|dir| dir.to_string_lossy().into_owned())
            .unwrap_or_else(|_| ".".to_owned());
        let specs = self.probe_specs();
        let reports = self.catalog_reports_tx.clone();
        let verified_tx = self.provider_lists_tx.clone();
        let host_id = host_id.clone();
        tokio::spawn(async move {
            let mut probes = tokio::task::JoinSet::new();
            for (index, spec) in specs.iter().enumerate() {
                let transport = match spec.launch {
                    ProviderLaunch::AcpStdio => crate::acp::session::Transport::Stdio {
                        command: spec.command.clone(),
                        args: spec.args.clone(),
                    },
                    ProviderLaunch::AcpEmbeddedPi => crate::acp::session::Transport::EmbeddedPi {
                        command: spec.command.clone(),
                        args: spec.args.clone(),
                    },
                };
                let provider_id = spec.name.clone();
                let opens_session = crate::discovery::needs_catalog_session(&provider_id);
                let host_id = host_id.clone();
                let reports = reports.clone();
                let cwd = cwd.clone();
                let spec = spec.clone();
                probes.spawn(async move {
                    let outcome = if opens_session {
                        crate::acp::catalog::read_catalog(
                            transport,
                            cwd,
                            DEFAULT_CATALOG_PROBE_BUDGET,
                        )
                        .await
                    } else {
                        crate::acp::catalog::verify_agent(
                            transport,
                            cwd,
                            DEFAULT_CATALOG_PROBE_BUDGET,
                        )
                        .await
                    };
                    (index, spec, provider_id, host_id, reports, outcome)
                });
            }

            // The list is rebuilt in the table's order every time an answer
            // arrives, so the report is a preference-ordered set of what has
            // been verified *so far* — never a set of what has merely been
            // tried. The final report, after the loop, is the complete one.
            let mut settled: Vec<Option<ProviderSpec>> = vec![None; specs.len()];
            while let Some(joined) = probes.join_next().await {
                let Ok((index, spec, provider_id, host_id, reports, outcome)) = joined else {
                    continue;
                };
                match outcome {
                    crate::acp::catalog::CatalogProbeOutcome::Read(catalog) => {
                        if catalog.is_empty() {
                            eprintln!(
                                "loom-worker: {provider_id} answered ACP but published no model \
                                 catalogue"
                            );
                        } else {
                            let _ = reports
                                .send(ProviderCatalogReport {
                                    host_id,
                                    provider_id,
                                    catalog,
                                })
                                .await;
                        }
                        settled[index] = Some(spec);
                    }
                    crate::acp::catalog::CatalogProbeOutcome::Failed { error } => {
                        eprintln!(
                            "loom-worker: {provider_id} did not answer ACP and is not offered: \
                             {error}"
                        );
                    }
                }
                let verified = settled.iter().flatten().cloned().collect();
                let _ = verified_tx.send(verified).await;
            }
            // A machine whose every candidate failed — including one with no
            // candidates at all — still says so, because an empty list is the
            // fact that clears whatever it reported before.
            let _ = verified_tx
                .send(settled.into_iter().flatten().collect())
                .await;
        });
    }

    /// The specs to probe for this connection.
    ///
    /// Everything installed here, plus any agent the server named that the
    /// table does not know: a server-declared agent still deserves a catalogue,
    /// even though its presence is not evidence of a local installation.
    fn probe_specs(&self) -> Vec<ProviderSpec> {
        effective_specs(
            &self.discovered,
            &self.providers,
            self.config.provider.as_ref(),
        )
    }

    /// Sends one heartbeat. Frames are not awaited: heartbeats carry no reply
    /// the execution plane needs, and `run` drains the socket.
    pub async fn heartbeat(&mut self) -> Result<(), WorkerError> {
        let host_id = self
            .host_id
            .clone()
            .ok_or_else(|| WorkerError::Protocol("not enrolled yet".into()))?;
        self.send(&ClientCommand::HostHeartbeat { host_id }).await
    }

    /// Runs until the socket closes: heartbeats on an interval, absorbs
    /// incoming frames, starts providers for dispatches, and forwards the
    /// reports those providers produce.
    ///
    /// Cancelling the future leaves the socket open; call [`Worker::disconnect`]
    /// to announce the departure before dropping it.
    pub async fn run(&mut self) -> Result<(), WorkerError> {
        let mut ticker = tokio::time::interval(self.config.heartbeat_interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // The first tick is immediate; the connection is fresh, so skip it.
        ticker.tick().await;

        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    self.heartbeat().await?;
                }
                message = self.socket.next() => {
                    match message {
                        // The connection dropped. The processes do not: a
                        // terminal is the user's, not the connection's, so the
                        // sessions are marked undrivable and reconciled when the
                        // worker reconnects.
                        None => {
                            self.terminal_sessions
                                .mark_all_disconnected(loom_relay::now_ms());
                            return Ok(());
                        }
                        Some(message) => self.on_socket_message(message?)?,
                    }
                }
                report = self.reports.recv() => {
                    let Some(report) = report else { continue };
                    if report.event.is_terminal() {
                        self.running.remove(&report.event.run_id);
                        // A turn that ended cannot still be blocked on a
                        // question: settle the requests *this run* left open as
                        // cancelled, so its agent is not left waiting for an
                        // answer the run no longer has a place for. Scoped by
                        // run because other threads may be running concurrently.
                        let settled = self
                            .permissions
                            .cancel_run(
                                &report.event.run_id,
                                "the run ended before the request was answered",
                            )
                            .await;
                        if settled > 0 {
                            eprintln!(
                                "loom-worker: settled {settled} permission request(s) as cancelled \
                                 because the run ended"
                            );
                        }
                    }
                    self.send(&ClientCommand::RunReport {
                        report: Box::new(report),
                    })
                    .await?;
                }
                report = self.catalog_reports.recv() => {
                    let Some(report) = report else { continue };
                    self.send(&ClientCommand::CatalogReport { report }).await?;
                }
                report = self.command_reports.recv() => {
                    let Some(report) = report else { continue };
                    self.send(&ClientCommand::CommandsReport { report }).await?;
                }
                providers = self.provider_lists.recv() => {
                    let Some(providers) = providers else { continue };
                    // Enrollment precedes the probe run that produces this list,
                    // so the host id is known here. A worker whose probes finish
                    // before it enrolled has nothing to report them against and
                    // drops the list rather than sending an unattributable one.
                    let Some(host_id) = self.host_id.clone() else { continue };
                    self.send(&ClientCommand::HostProviders { host_id, providers }).await?;
                }
                request = self.interactions.recv() => {
                    let Some(request) = request else { continue };
                    self.send(&ClientCommand::InteractionRequest {
                        request: Box::new(request),
                    })
                    .await?;
                }
                report = self.env_reports.recv() => {
                    let Some(report) = report else { continue };
                    self.send(&ClientCommand::EnvironmentReport { report }).await?;
                }
                report = self.env_deprovision_reports.recv() => {
                    let Some(report) = report else { continue };
                    self.send(&ClientCommand::EnvironmentDeprovisionReport { report })
                        .await?;
                }
                report = self.host_file_reports.recv() => {
                    let Some(report) = report else { continue };
                    self.send(&ClientCommand::HostFileReport { report }).await?;
                }
                report = self.host_rpc_reports.recv() => {
                    let Some(report) = report else { continue };
                    self.send(&ClientCommand::HostRpcReport { report }).await?;
                }
                report = self.history_reports.recv() => {
                    let Some(report) = report else { continue };
                    self.send(&ClientCommand::HistoryReport { report }).await?;
                }
                report = self.terminal_reports.recv() => {
                    let Some(report) = report else { continue };
                    self.send(&ClientCommand::TerminalReport { report }).await?;
                }
                report = self.script_reports.recv() => {
                    let Some(report) = report else { continue };
                    self.send(&ClientCommand::ScriptReport { report }).await?;
                }
            }
        }
    }

    /// Announces the departure and closes the socket.
    ///
    /// A clean shutdown kills every terminal this worker holds: they are its
    /// child processes, and leaving them running with no worker to report them
    /// would leak processes the control plane could never see again. A dropped
    /// connection does **not** go through here — a terminal is the user's, not
    /// the connection's, so those sessions survive a reconnect and are
    /// reconciled then.
    pub async fn disconnect(mut self) -> Result<(), WorkerError> {
        let closing = self.terminal_sessions.len();
        self.terminal_sessions.close_all(loom_relay::now_ms());
        if closing > 0 {
            eprintln!("loom-worker: closed {closing} terminal session(s) on shutdown");
        }
        if let Some(host_id) = self.host_id.clone() {
            let _ = self.send(&ClientCommand::HostDisconnect { host_id }).await;
        }
        let _ = self.socket.close(None).await;
        Ok(())
    }

    /// The host id, once enrolled.
    pub fn host_id(&self) -> Option<&HostId> {
        self.host_id.as_ref()
    }

    /// The configured display name.
    pub fn name(&self) -> &str {
        &self.config.name
    }

    /// The highest host-scope event id applied so far.
    ///
    /// A worker that restarts can persist this and pass it back as
    /// [`WorkerConfig::resume_cursor`], so it replays exactly what it missed
    /// instead of the whole retained window.
    pub fn cursor(&self) -> Option<&EventId> {
        self.cursor.as_ref()
    }

    /// The run ids with a provider task in flight.
    pub fn running_runs(&self) -> usize {
        self.running.len()
    }

    /// Handles one frame from the server.
    fn on_socket_message(&mut self, message: Message) -> Result<(), WorkerError> {
        match message {
            Message::Text(text) => self.on_server_frame(serde_json::from_str(text.as_str())?),
            Message::Binary(bytes) => self.on_server_frame(serde_json::from_slice(&bytes)?),
            Message::Close(_) => Err(WorkerError::Protocol("server closed the socket".into())),
            Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => Ok(()),
        }
    }

    /// Handles one decoded server message, starting a provider for dispatches.
    fn on_server_frame(&mut self, message: ServerMessage) -> Result<(), WorkerError> {
        match message {
            ServerMessage::Event {
                event_id,
                scope,
                payload,
                ..
            } => self.observe_event(&event_id, &scope, &payload),
            ServerMessage::Error { message } => Err(WorkerError::Protocol(message)),
            // A question the control plane refused to record has no client that
            // can ever answer it — the interaction entity a UI renders and
            // answers was never created. Waiting out the permission timeout
            // would show the user a turn blocked on a question nobody can see,
            // so the agent is told "not granted" now, and the reason is logged
            // because it is the only account of why the question vanished.
            ServerMessage::InteractionRequestAck {
                request_id,
                accepted: false,
                detail,
                ..
            } => {
                let registry = self.permissions.clone();
                let reason =
                    detail.unwrap_or_else(|| "the control plane refused the request".to_owned());
                tokio::spawn(async move {
                    if registry.refuse(&request_id, &reason).await {
                        eprintln!(
                            "loom-worker: the control plane refused permission request \
                             {request_id} ({reason}); the agent is told it was not granted"
                        );
                    } else {
                        eprintln!(
                            "loom-worker: the control plane refused permission request \
                             {request_id} ({reason}), but nothing was waiting for it"
                        );
                    }
                });
                Ok(())
            }
            // Hello, accepted acknowledgements and pongs are the transport
            // shell's to ignore: a recorded question is answered by its
            // resolution, which arrives as an event.
            _ => Ok(()),
        }
    }

    /// Records an event id and starts a run if the payload is a dispatch.
    fn observe_event(
        &mut self,
        event_id: &str,
        scope: &Scope,
        payload: &str,
    ) -> Result<(), WorkerError> {
        let parsed = event_id.parse::<EventId>().ok();
        if let Some(id) = &parsed {
            // Advance the resume cursor monotonically; replay and live traffic
            // can arrive in either order around a reconnect.
            if self.cursor.as_ref().is_none_or(|cursor| id > cursor) {
                self.cursor = Some(*id);
            }
            if !self.seen_events.insert(*id) {
                return Ok(());
            }
        }

        let Scope::Host(host_id) = scope else {
            return Ok(());
        };
        if Some(host_id) != self.host_id.as_ref().map(ToString::to_string).as_ref() {
            return Ok(());
        }

        // Only a dispatch, an interaction resolution, a teardown, a
        // provisioning request, a host file request, or a workspace request
        // parses as one; host domain events in the same room are none of them.
        if let Ok(dispatch) = serde_json::from_str::<RunDispatch>(payload) {
            self.start_dispatch(dispatch);
        } else if let Ok(steer) = serde_json::from_str::<RunSteer>(payload) {
            self.steer_run(steer);
        } else if let Ok(resolution) = serde_json::from_str::<InteractionResolutionFrame>(payload) {
            self.resolve_interaction(resolution);
        } else if let Ok(deprovision) = serde_json::from_str::<EnvironmentDeprovision>(payload) {
            // Before `EnvironmentProvision`, deliberately: a provisioning
            // payload cannot decode as a deprovision (its `path` is required),
            // but the reverse would succeed and a teardown would be ignored.
            self.start_deprovision(deprovision);
        } else if let Ok(provision) = serde_json::from_str::<EnvironmentProvision>(payload) {
            self.start_provision(provision);
        } else if let Ok(request) = serde_json::from_str::<HostFileRequest>(payload) {
            self.start_host_file_request(request);
        } else if let Ok(request) = serde_json::from_str::<HostRpcRequest>(payload) {
            self.start_host_rpc_request(request);
        } else if let Ok(request) =
            serde_json::from_str::<loom_provider_protocol::TerminalRequest>(payload)
        {
            self.start_terminal_request(request);
        } else if let Ok(dispatch) =
            serde_json::from_str::<loom_provider_protocol::ScriptRunDispatch>(payload)
        {
            self.start_script_run(dispatch);
        } else if let Ok(cancel) =
            serde_json::from_str::<loom_provider_protocol::ScriptRunCancel>(payload)
        {
            self.cancel_script_run(cancel);
        }
        Ok(())
    }

    /// Starts an automation script on this machine.
    ///
    /// The work runs on its own task: a script can take minutes, and the socket
    /// loop must keep forwarding heartbeats and other runs' reports while it
    /// does.
    fn start_script_run(&self, dispatch: loom_provider_protocol::ScriptRunDispatch) {
        let Some(scripts) = self.scripts.clone() else {
            return;
        };
        tokio::spawn(async move {
            scripts.start(dispatch).await;
        });
    }

    /// Kills a script this machine is running.
    fn cancel_script_run(&self, cancel: loom_provider_protocol::ScriptRunCancel) {
        let Some(scripts) = self.scripts.clone() else {
            return;
        };
        tokio::spawn(async move {
            scripts.cancel(&cancel).await;
        });
    }

    /// Executes one terminal operation on the worker's machine.
    ///
    /// The work runs on the blocking pool: a pty read or a shell spawn is
    /// synchronous, and running it on the socket loop would stall every other
    /// session this worker serves.
    fn start_terminal_request(&self, request: loom_provider_protocol::TerminalRequest) {
        if self.host_id.as_ref() != Some(&request.host_id) {
            return;
        }
        let reports = self.terminal_reports_tx.clone();
        let sessions = self.terminal_sessions.clone();
        tokio::spawn(async move {
            let report =
                tokio::task::spawn_blocking(move || crate::terminal::answer(request, &sessions))
                    .await
                    .unwrap_or_else(|error| {
                        eprintln!("loom-worker: terminal request panicked: {error}");
                        loom_provider_protocol::TerminalReport {
                            host_id: loom_domain::HostId::mint(),
                            request_id: String::new(),
                            outcome: loom_provider_protocol::TerminalOutcome::Failed {
                                code: "internal_error".into(),
                                message: "the terminal request panicked".into(),
                            },
                        }
                    });
            let _ = reports.send(report).await;
        });
    }

    /// Executes one workspace operation on the worker's machine.
    fn start_host_rpc_request(&self, request: HostRpcRequest) {
        if self.host_id.as_ref() != Some(&request.host_id) {
            return;
        }
        // A history load is not a workspace operation: it opens a provider
        // session rather than a path on this machine.
        if matches!(request.operation, HostRpcOperation::LoadHistory { .. }) {
            self.start_history_load(request);
            return;
        }
        let reports = self.host_rpc_reports_tx.clone();
        let default_root = self.config.environment_root.clone();
        tokio::spawn(async move {
            let report = workspace::answer_with_root(request, default_root).await;
            let _ = reports.send(report).await;
        });
    }

    /// Streams one thread's restored conversation back to the server.
    ///
    /// The answer is frames, not one report: a batch per bounded slice, then a
    /// terminator. A failed load sends `Failed` and **no** `Complete`, so a
    /// broken read can never be mistaken for a short conversation.
    fn start_history_load(&self, request: HostRpcRequest) {
        let reports = self.history_reports_tx.clone();
        let host_id = request.host_id.clone();
        let request_id = request.request_id.clone();
        let HostRpcOperation::LoadHistory {
            thread_id,
            provider,
            provider_session_id,
            cwd,
            max_batch_bytes,
            max_total_bytes,
        } = request.operation
        else {
            return;
        };

        tokio::spawn(async move {
            let transport = match provider.launch {
                ProviderLaunch::AcpStdio => crate::acp::session::Transport::Stdio {
                    command: provider.command.clone(),
                    args: provider.args.clone(),
                },
                ProviderLaunch::AcpEmbeddedPi => crate::acp::session::Transport::EmbeddedPi {
                    command: provider.command.clone(),
                    args: provider.args.clone(),
                },
            };
            let frames = match crate::acp::history::load_history(
                transport,
                cwd,
                thread_id,
                provider_session_id,
                crate::acp::history::HistoryLimits {
                    max_total_bytes,
                    budget: DEFAULT_HISTORY_LOAD_BUDGET,
                },
            )
            .await
            {
                Ok(entries) => crate::acp::history::into_frames(entries, max_batch_bytes),
                Err(failure) => vec![HistoryPart::Failed {
                    code: failure.code.to_owned(),
                    message: failure.message,
                }],
            };
            for part in frames {
                let report = HistoryReport {
                    host_id: host_id.clone(),
                    request_id: request_id.clone(),
                    part,
                };
                if reports.send(report).await.is_err() {
                    return;
                }
            }
        });
    }

    /// Hands a permission answer to the provider task waiting for it.
    ///
    /// A resolution for a request this worker is not holding is a no-op rather
    /// than an error: the relay may replay a frame the worker already applied,
    /// and a frame for a run that ended while the answer was in flight has
    /// nowhere to go. Neither is a failure, and treating one as a failure would
    /// make a redelivered frame fatal.
    fn resolve_interaction(&self, resolution: InteractionResolutionFrame) {
        // A frame addressed to a different host is not this worker's: the relay
        // room should make that impossible, but checking costs nothing and a
        // panic on an unexpected frame would take the whole connection down.
        if self.host_id.as_ref() != Some(&resolution.host_id) {
            return;
        }
        let registry = self.permissions.clone();
        tokio::spawn(async move {
            if !registry
                .resolve(&resolution.request_id, resolution.answer)
                .await
            {
                eprintln!(
                    "loom-worker: an answer for permission request {} arrived with nothing \
                     waiting for it; dropping it",
                    resolution.request_id
                );
            }
        });
    }

    /// Starts a provider for a dispatch unless it was already started.
    fn start_dispatch(&mut self, dispatch: RunDispatch) {
        if self.seen_runs.contains(&dispatch.run_id) || self.running.contains(&dispatch.run_id) {
            return;
        }
        self.seen_runs.insert(&dispatch.run_id);
        self.running.insert(dispatch.run_id.clone());

        // An operator override replaces the *executable*, never the workspace:
        // the environment decides where a provider runs, the machine decides
        // which binary. Carrying `cwd` across the override is what keeps a
        // `LOOM_PROVIDER_CMD` from silently running in the worker's own cwd.
        let spec = match &self.config.provider {
            Some(override_spec) => {
                let mut spec = override_spec.clone();
                spec.cwd = dispatch.provider.cwd.clone();
                spec
            }
            None => dispatch.provider.clone(),
        };
        let run = ProviderRun::from_dispatch(
            &dispatch,
            spec,
            self.config.run_timeout,
            self.config.run_ceiling,
            self.config.permission_timeout,
            self.config.settle_timeout,
        );
        // ACP is the only provider protocol. Pi uses the embedded adapter;
        // native agents use the same client over their stdio transport. The
        // dispatch, reconciliation and relay remain unaware of that detail.
        let permissions = self.permissions.clone();
        let interactions = self.interactions_tx.clone();
        let steers = self.steers.clone();
        match run.spec.launch {
            loom_provider_protocol::ProviderLaunch::AcpStdio => {
                let transport = crate::acp::session::Transport::Stdio {
                    command: run.spec.command.clone(),
                    args: run.spec.args.clone(),
                };
                crate::acp::session::spawn(
                    run,
                    transport,
                    self.reports_tx.clone(),
                    self.catalog_reports_tx.clone(),
                    self.command_reports_tx.clone(),
                    permissions,
                    interactions,
                    steers,
                );
            }
            loom_provider_protocol::ProviderLaunch::AcpEmbeddedPi => {
                let transport = crate::acp::session::Transport::EmbeddedPi {
                    command: run.spec.command.clone(),
                    args: run.spec.args.clone(),
                };
                crate::acp::session::spawn(
                    run,
                    transport,
                    self.reports_tx.clone(),
                    self.catalog_reports_tx.clone(),
                    self.command_reports_tx.clone(),
                    permissions,
                    interactions,
                    steers,
                );
            }
        }
    }

    /// Hands a steer to the run's conversation loop.
    ///
    /// The loop is the only thing that can cancel and re-prompt its own ACP
    /// session, so this is a hand-off, not an action. A steer for a run that is
    /// not live — one that already ended, or that this worker never started —
    /// is dropped: the relay may replay a frame the worker already applied, and
    /// a steer can lose a race with the run's own terminal event, and neither
    /// is a failure.
    fn steer_run(&self, steer: RunSteer) {
        if self.host_id.as_ref() != Some(&steer.host_id) {
            return;
        }
        let steers = self.steers.clone();
        tokio::spawn(async move {
            if !steers.steer(&steer.run_id, steer.text).await {
                eprintln!(
                    "loom-worker: a steer for run {} arrived with no live turn; dropping it",
                    steer.run_id
                );
            }
        });
    }

    /// Creates a managed environment's workspace in the background and queues
    /// the report for the socket loop to forward.
    ///
    /// Provisioning is idempotent in both shapes. A directory creation accepts
    /// an existing directory; a worktree is re-adopted when it already sits on
    /// the expected branch, and its include copy is redone when the completion
    /// marker is missing. The relay's per-connection dedup already suppresses a
    /// replayed event id; the explicit re-adoption covers a retry the user
    /// asks for after a failure.
    fn start_provision(&mut self, provision: EnvironmentProvision) {
        let root = self.config.environment_root.clone();
        let reports = self.env_reports_tx.clone();
        tokio::spawn(async move {
            let outcome = provision_environment(&root, &provision).await;
            let _ = reports
                .send(EnvironmentProvisionReport {
                    host_id: provision.host_id.clone(),
                    environment_id: provision.environment_id.clone(),
                    outcome,
                })
                .await;
        });
    }

    /// Removes a managed environment's workspace in the background and queues
    /// the report for the socket loop to forward.
    ///
    /// The path is one this worker reported earlier, so a removal is scoped to
    /// the workspace root as a second line of defence; a path outside it is
    /// refused rather than followed.
    fn start_deprovision(&mut self, deprovision: EnvironmentDeprovision) {
        let root = self.config.environment_root.clone();
        let reports = self.env_deprovision_reports_tx.clone();
        tokio::spawn(async move {
            let outcome = deprovision_environment(&root, &deprovision).await;
            let _ = reports
                .send(EnvironmentDeprovisionReport {
                    host_id: deprovision.host_id.clone(),
                    environment_id: deprovision.environment_id.clone(),
                    outcome,
                })
                .await;
        });
    }

    /// Subscribes and waits for the acknowledgement.
    async fn subscribe(&mut self, scope: Scope) -> Result<(), WorkerError> {
        self.send(&ClientCommand::Subscribe { scope }).await?;
        loop {
            match next_message(&mut self.socket).await? {
                ServerMessage::Subscribed { .. } => return Ok(()),
                ServerMessage::Error { message } => return Err(WorkerError::Protocol(message)),
                _ => continue,
            }
        }
    }

    /// Requests the backlog for the host scope and applies every dispatch in
    /// it.
    ///
    /// Pages until the server reports the head. One page is not enough: a
    /// reconnect after a burst can leave more dispatches queued than the page
    /// limit, and the server returns the *oldest* frames after the cursor
    /// precisely so that repeating the request with the advanced cursor cannot
    /// skip any. Stopping at the first page would advance the cursor past the
    /// dispatches that did not fit, and they would never be retried.
    async fn replay_host_scope(&mut self, host_id: &HostId) -> Result<(), WorkerError> {
        let scope = Scope::Host(host_id.to_string());
        loop {
            self.send(&ClientCommand::Replay {
                scope: scope.clone(),
                since: self.cursor,
                limit: Some(self.config.replay_limit),
            })
            .await?;

            // `ReplayComplete` carries `has_more`, so read the page and decide
            // from the server's answer rather than inferring from `count`.
            let has_more = loop {
                match next_message(&mut self.socket).await? {
                    ServerMessage::Event {
                        event_id,
                        scope,
                        payload,
                        ..
                    } => self.observe_event(&event_id, &scope, &payload)?,
                    ServerMessage::ReplayComplete { has_more, .. } => break has_more,
                    ServerMessage::Error { message } => return Err(WorkerError::Protocol(message)),
                    _ => continue,
                }
            };

            if !has_more {
                return Ok(());
            }
        }
    }

    async fn send(&mut self, command: &ClientCommand) -> Result<(), WorkerError> {
        let text = serde_json::to_string(command)?;
        self.socket.send(Message::Text(text.into())).await?;
        Ok(())
    }
}

/// The agents a worker offers, given what it found and what it was told.
///
/// Discovery leads: an agent installed here is offered whatever the control
/// plane believes. Then any agent the server named that discovery did not find
/// is appended, because a server-declared agent is still dispatchable even when
/// the table does not know it.
///
/// The operator override replaces the **default** agent's executable, because
/// that is the one dispatch hands a run when the caller names no provider. A
/// machine that pins one binary therefore keeps its single-provider behaviour
/// for the default, while the agents discovery found stay selectable.
///
/// An empty result with an override means nothing was found and nothing was
/// advertised: the override is then the only agent, which is the
/// single-provider shape loom had before discovery.
fn effective_specs(
    discovered: &[ProviderSpec],
    advertised: &[ProviderSpec],
    override_spec: Option<&ProviderSpec>,
) -> Vec<ProviderSpec> {
    let mut specs = discovered.to_vec();
    for spec in advertised {
        if !specs.iter().any(|found| found.name == spec.name) {
            specs.push(spec.clone());
        }
    }
    if specs.is_empty() {
        if let Some(override_spec) = override_spec {
            specs.push(override_spec.clone());
        }
    }
    if let Some(override_spec) = override_spec {
        if let Some(default) = specs.first_mut() {
            default.launch = override_spec.launch;
            default.command = override_spec.command.clone();
            default.args = override_spec.args.clone();
        }
    }
    specs
}

/// Creates one managed environment's workspace under `root`.
///
/// The directory is `<root>/<environment_id>`. With no workspace selection
/// that is an empty directory, as it was before worktrees existed; a
/// [`EnvironmentProvisionWorkspace::GitWorktree`] turns it into a checkout of
/// the named branch cut from the project source. A failure carries the path and
/// the reason so it survives to the UI.
async fn provision_environment(
    root: &Path,
    provision: &EnvironmentProvision,
) -> EnvironmentProvisionOutcome {
    if let Some(EnvironmentProvisionWorkspace::GitWorktree {
        source_path,
        branch_name,
        base_branch,
    }) = provision.workspace.as_ref()
    {
        return worktree::provision(
            root,
            &provision.environment_id,
            source_path,
            branch_name,
            base_branch.as_deref(),
        )
        .await;
    }
    let path = root.join(provision.environment_id.to_string());
    match tokio::fs::create_dir_all(&path).await {
        Ok(()) => EnvironmentProvisionOutcome::Provisioned {
            path: path.to_string_lossy().into_owned(),
            branch_name: None,
            base_branch: None,
            default_branch: None,
            is_git_repo: None,
        },
        Err(error) => EnvironmentProvisionOutcome::Failed {
            error: format!("could not create {}: {error}", path.display()),
        },
    }
}

/// Removes one managed environment's workspace under `root`.
///
/// The path recorded by the server is used when present; an environment that
/// never reached `ready` has none, so the worker's own layout for the id is the
/// fallback. A path outside `root` is refused: the worker only ever removes
/// what it created.
async fn deprovision_environment(
    root: &Path,
    deprovision: &EnvironmentDeprovision,
) -> EnvironmentDeprovisionOutcome {
    let recorded = deprovision.path.trim();
    let path = if recorded.is_empty() {
        root.join(deprovision.environment_id.to_string())
    } else {
        PathBuf::from(recorded)
    };
    if !path.starts_with(root) {
        return EnvironmentDeprovisionOutcome::Failed {
            error: format!(
                "refusing to remove {}: it is outside the workspace root {}",
                path.display(),
                root.display()
            ),
        };
    }
    match worktree::remove(&path).await {
        Ok(()) => EnvironmentDeprovisionOutcome::Removed,
        Err(error) => EnvironmentDeprovisionOutcome::Failed { error },
    }
}

async fn next_message(socket: &mut Socket) -> Result<ServerMessage, WorkerError> {
    loop {
        let message = socket
            .next()
            .await
            .ok_or_else(|| WorkerError::Protocol("server closed the socket".into()))??;
        match message {
            Message::Text(text) => return Ok(serde_json::from_str(text.as_str())?),
            Message::Binary(bytes) => return Ok(serde_json::from_slice(&bytes)?),
            Message::Close(_) => {
                return Err(WorkerError::Protocol("server closed the socket".into()));
            }
            Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => continue,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_http_url_becomes_a_websocket_url() {
        let config = WorkerConfig::new("http://127.0.0.1:38886", "laptop");
        assert_eq!(config.websocket_url(), "ws://127.0.0.1:38886/internal/ws");

        let config = WorkerConfig::new("https://loom.example.com/", "laptop");
        assert_eq!(config.websocket_url(), "wss://loom.example.com/internal/ws");

        let config = WorkerConfig::new("ws://host:9/ws", "laptop");
        assert_eq!(config.websocket_url(), "ws://host:9/internal/ws");

        let config = WorkerConfig::new("host:1234", "laptop");
        assert_eq!(config.websocket_url(), "ws://host:1234/internal/ws");
    }

    #[test]
    fn a_mismatched_server_protocol_version_is_refused() {
        assert!(ensure_compatible_protocol(loom_server::PROTOCOL_VERSION).is_ok());
        let error = ensure_compatible_protocol(loom_server::PROTOCOL_VERSION + 1)
            .expect_err("a newer server must be refused");
        assert!(error.to_string().contains("protocol version"), "{error}");
    }

    #[test]
    fn the_run_dedup_set_is_bounded_and_idempotent() {
        let mut seen = RunSeen::new(2);
        let first = RunId::mint();
        let second = RunId::mint();
        let third = RunId::mint();

        assert!(seen.insert(&first));
        assert!(!seen.insert(&first), "a redelivery is not a new run");
        assert!(seen.insert(&second));
        assert!(seen.insert(&third));
        // `first` was evicted, but a duplicate that old is not expected: the
        // relay window is far larger than the dedup capacity.
        assert!(!seen.contains(&first));
        assert!(seen.contains(&third));
    }

    #[tokio::test]
    async fn provisioning_creates_the_workspace_and_reports_its_path() {
        let root = tempfile::tempdir().unwrap();
        let provision = EnvironmentProvision {
            environment_id: loom_domain::EnvironmentId::mint(),
            project_id: loom_domain::ProjectId::mint(),
            host_id: HostId::mint(),
            created_at_ms: 1,
            workspace: None,
        };

        let outcome = provision_environment(root.path(), &provision).await;
        let EnvironmentProvisionOutcome::Provisioned { path, .. } = outcome else {
            panic!("expected a provisioned workspace, got {outcome:?}");
        };
        assert_eq!(
            path,
            root.path()
                .join(provision.environment_id.to_string())
                .to_string_lossy()
        );
        assert!(Path::new(&path).is_dir());

        // Idempotent: a replay of the same request is not an error.
        assert!(matches!(
            provision_environment(root.path(), &provision).await,
            EnvironmentProvisionOutcome::Provisioned { .. }
        ));
    }

    #[tokio::test]
    async fn provisioning_under_a_non_directory_root_reports_why() {
        let root = tempfile::tempdir().unwrap();
        // A regular file where the root directory should be: every attempt to
        // create a child fails, and the reason must name the path.
        let file = root.path().join("not-a-dir");
        std::fs::write(&file, "x").unwrap();
        let provision = EnvironmentProvision {
            environment_id: loom_domain::EnvironmentId::mint(),
            project_id: loom_domain::ProjectId::mint(),
            host_id: HostId::mint(),
            created_at_ms: 1,
            workspace: None,
        };

        let outcome = provision_environment(&file, &provision).await;
        let EnvironmentProvisionOutcome::Failed { error } = outcome else {
            panic!("expected a failure, got {outcome:?}");
        };
        assert!(error.contains("not-a-dir"), "{error}");
    }

    fn spec(name: &str, command: &str) -> ProviderSpec {
        ProviderSpec {
            name: name.to_owned(),
            launch: loom_provider_protocol::ProviderLaunch::AcpStdio,
            command: command.to_owned(),
            args: Vec::new(),
            cwd: None,
        }
    }

    /// An agent found on this machine is offered, and one the server merely
    /// listed is appended rather than dropped.
    #[test]
    fn discovery_leads_and_advertised_agents_are_kept() {
        let discovered = vec![spec("pi", "/usr/bin/pi"), spec("omp", "/usr/bin/omp")];
        let advertised = vec![spec("pi", "pi"), spec("codex", "codex")];
        let probed = effective_specs(&discovered, &advertised, None);
        assert_eq!(
            probed
                .iter()
                .map(|spec| spec.name.as_str())
                .collect::<Vec<_>>(),
            vec!["pi", "omp", "codex"],
            "the installed agent wins its name, the declared one is still probed"
        );
        assert_eq!(
            probed[0].command, "/usr/bin/pi",
            "the local executable is the one that was found"
        );
    }

    /// An operator override replaces the default agent's executable, and only
    /// that one: a pinned machine keeps its single-provider behaviour without
    /// breaking the agents discovery found.
    #[test]
    fn the_override_replaces_only_the_default_agents_executable() {
        let discovered = vec![spec("pi", "/usr/bin/pi"), spec("omp", "/usr/bin/omp")];
        let override_spec = spec("acp", "/opt/custom/agent");
        let probed = effective_specs(&discovered, &[], Some(&override_spec));

        assert_eq!(probed[0].name, "pi", "the default keeps its identity");
        assert_eq!(probed[0].command, "/opt/custom/agent");
        assert_eq!(probed[1].name, "omp");
        assert_eq!(probed[1].command, "/usr/bin/omp", "a sibling is left alone");
    }

    /// A machine with nothing installed still honours an explicit command:
    /// the operator's agent is the only one, which is the single-provider shape
    /// loom had before discovery.
    #[test]
    fn an_empty_machine_falls_back_to_the_operator_command() {
        let override_spec = spec("acp", "/opt/custom/agent");
        let probed = effective_specs(&[], &[], Some(&override_spec));
        assert_eq!(probed.len(), 1);
        assert_eq!(probed[0].command, "/opt/custom/agent");

        assert!(
            effective_specs(&[], &[], None).is_empty(),
            "nothing found and nothing declared means no agent is offered"
        );
    }

    /// A server that declares an agent discovery does not know still gets it
    /// probed: the declaration is the only evidence the agent exists.
    #[test]
    fn a_declared_agent_is_probed_on_a_machine_that_found_nothing() {
        let advertised = vec![spec("pi", "pi")];
        let probed = effective_specs(&[], &advertised, None);
        assert_eq!(
            probed
                .iter()
                .map(|spec| spec.name.as_str())
                .collect::<Vec<_>>(),
            vec!["pi"]
        );
    }

    /// An agent a sibling host offers is probed here too: the server's hello
    /// carries every host's list, and probing one names the executable that host
    /// uses. It only reaches the report if it answers on this machine as well,
    /// which is the evidence a report needs.
    #[test]
    fn another_hosts_agent_is_probed_too() {
        let discovered = vec![spec("pi", "/usr/bin/pi")];
        let advertised = vec![spec("pi", "pi"), spec("omp", "/elsewhere/omp")];
        let probed = effective_specs(&discovered, &advertised, None);
        assert_eq!(
            probed
                .iter()
                .map(|spec| spec.name.as_str())
                .collect::<Vec<_>>(),
            vec!["pi", "omp"]
        );
        assert_eq!(
            probed[1].command, "/elsewhere/omp",
            "the sibling's executable is what is tried, not a guess at a local one"
        );
    }
}
