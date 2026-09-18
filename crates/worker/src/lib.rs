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
pub mod host_files;
pub mod provider;
pub mod run;
pub mod scripts;
pub mod session;
pub mod terminal;
pub mod update;
pub mod workspace;

use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use loom_domain::{HostId, RunId};
use loom_provider_protocol::{
    EnvironmentProvision, EnvironmentProvisionOutcome, EnvironmentProvisionReport, HostFileRequest,
    HostRpcReport, HostRpcRequest, InteractionRequest, InteractionResolutionFrame, ProviderSpec,
    RunDispatch,
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
pub const DEFAULT_RUN_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// How many dispatch ids the worker remembers to suppress redelivery.
pub const DISPATCH_DEDUP_CAPACITY: usize = 512;

/// How many reports may be queued before a provider task waits.
const REPORT_CHANNEL_CAPACITY: usize = 256;

/// How long a permission request waits for a user by default.
///
/// Re-exported from the ACP permission bridge, which is where the reasoning
/// lives; `main.rs` uses it for `--permission-timeout-ms`.
pub const DEFAULT_PERMISSION_TIMEOUT: Duration = crate::acp::permission::DEFAULT_PERMISSION_TIMEOUT;

/// Where managed environments' workspaces are created by default.
///
/// `LOOM_WORKSPACE_ROOT` overrides it; otherwise `$HOME/.loom/workspaces`, or
/// the system temp directory when there is no home. The worker owns this
/// layout: the control plane only learns the resulting path from the report.
///
/// Deliberately independent of [`default_data_dir`]: a data directory is where
/// a machine's *own* data lives, a workspace root is where work happens, and
/// defaulting one from the other would silently relocate every existing
/// deployment's workspaces the moment `LOOM_DATA_DIR` was set.
pub fn default_environment_root() -> PathBuf {
    if let Some(root) = std::env::var_os("LOOM_WORKSPACE_ROOT") {
        return PathBuf::from(root);
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".loom").join("workspaces");
    }
    std::env::temp_dir().join("loom-workspaces")
}

/// The worker's own data directory on this machine.
///
/// `LOOM_DATA_DIR` overrides it; otherwise `$HOME/.loom`, or the system temp
/// directory when there is no home. This is the root the worker reports at
/// enrollment and the one thread storage is named from, so the control plane
/// never has to guess where a machine keeps its data.
pub fn default_data_dir() -> PathBuf {
    if let Some(root) = std::env::var_os("LOOM_DATA_DIR") {
        return PathBuf::from(root);
    }
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
    /// How long one provider run may take before it is killed.
    pub run_timeout: Duration,
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
            run_timeout: DEFAULT_RUN_TIMEOUT,
            permission_timeout: DEFAULT_PERMISSION_TIMEOUT,
            environment_root: default_environment_root(),
            data_dir: default_data_dir(),
            join_code: None,
            resume_cursor: None,
            replay_limit: 500,
            update,
        }
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
    /// Environment-provisioning reports waiting to be forwarded to the server.
    env_reports: mpsc::Receiver<EnvironmentProvisionReport>,
    env_reports_tx: mpsc::Sender<EnvironmentProvisionReport>,
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
            ServerMessage::Hello { protocol_version } => {
                // Refuse a peer this build cannot speak to, before enrolling.
                // A mismatch after enrollment would corrupt dispatch/runs.
                ensure_compatible_protocol(protocol_version)?;
                let (reports_tx, reports) = mpsc::channel(REPORT_CHANNEL_CAPACITY);
                let (interactions_tx, interactions) = mpsc::channel(REPORT_CHANNEL_CAPACITY);
                let (env_reports_tx, env_reports) = mpsc::channel(REPORT_CHANNEL_CAPACITY);
                let (host_file_reports_tx, host_file_reports) =
                    mpsc::channel(REPORT_CHANNEL_CAPACITY);
                let (host_rpc_reports_tx, host_rpc_reports) =
                    mpsc::channel(REPORT_CHANNEL_CAPACITY);
                let (terminal_reports_tx, terminal_reports) =
                    mpsc::channel(REPORT_CHANNEL_CAPACITY);
                let (script_reports_tx, script_reports) = mpsc::channel(REPORT_CHANNEL_CAPACITY);
                Ok(Self {
                    socket,
                    cursor: config.resume_cursor,
                    config,
                    host_id: None,
                    seen_events: SeenSet::new(DISPATCH_DEDUP_CAPACITY),
                    seen_runs: RunSeen::new(DISPATCH_DEDUP_CAPACITY),
                    reports,
                    reports_tx,
                    interactions,
                    interactions_tx,
                    permissions: crate::acp::permission::PermissionRegistry::new(),
                    env_reports,
                    env_reports_tx,
                    host_file_reports,
                    host_file_reports_tx,
                    host_rpc_reports,
                    host_rpc_reports_tx,
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
        Ok(host_id)
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
                report = self.host_file_reports.recv() => {
                    let Some(report) = report else { continue };
                    self.send(&ClientCommand::HostFileReport { report }).await?;
                }
                report = self.host_rpc_reports.recv() => {
                    let Some(report) = report else { continue };
                    self.send(&ClientCommand::HostRpcReport { report }).await?;
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

        // Only a dispatch, an interaction resolution, a provisioning request,
        // a host file request, or a workspace request parses as one; host
        // domain events in the same room are none of them.
        if let Ok(dispatch) = serde_json::from_str::<RunDispatch>(payload) {
            self.start_dispatch(dispatch);
        } else if let Ok(resolution) = serde_json::from_str::<InteractionResolutionFrame>(payload) {
            self.resolve_interaction(resolution);
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
        let reports = self.host_rpc_reports_tx.clone();
        let default_root = self.config.environment_root.clone();
        tokio::spawn(async move {
            let report = workspace::answer_with_root(request, default_root).await;
            let _ = reports.send(report).await;
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
            self.config.permission_timeout,
        );
        // ACP is the only provider protocol. Pi uses the embedded adapter;
        // native agents use the same client over their stdio transport. The
        // dispatch, reconciliation and relay remain unaware of that detail.
        let permissions = self.permissions.clone();
        let interactions = self.interactions_tx.clone();
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
                    permissions,
                    interactions,
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
                    permissions,
                    interactions,
                );
            }
        }
    }

    /// Creates a managed environment's workspace in the background and queues
    /// the report for the socket loop to forward.
    ///
    /// Provisioning is idempotent (`create_dir_all` accepts an existing
    /// directory), so a replay of the same request is safe even without a dedup
    /// set; the relay's own per-connection dedup already suppresses a replayed
    /// event id.
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

/// Creates one managed environment's workspace under `root`.
///
/// The directory is `<root>/<environment_id>`. Creation is idempotent, so a
/// redelivered provisioning request is harmless; a failure carries the path and
/// the OS error so the reason survives to the UI.
async fn provision_environment(
    root: &Path,
    provision: &EnvironmentProvision,
) -> EnvironmentProvisionOutcome {
    let path = root.join(provision.environment_id.to_string());
    match tokio::fs::create_dir_all(&path).await {
        Ok(()) => EnvironmentProvisionOutcome::Provisioned {
            path: path.to_string_lossy().into_owned(),
        },
        Err(error) => EnvironmentProvisionOutcome::Failed {
            error: format!("could not create {}: {error}", path.display()),
        },
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
        };

        let outcome = provision_environment(root.path(), &provision).await;
        let EnvironmentProvisionOutcome::Provisioned { path } = outcome else {
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
        };

        let outcome = provision_environment(&file, &provision).await;
        let EnvironmentProvisionOutcome::Failed { error } = outcome else {
            panic!("expected a failure, got {outcome:?}");
        };
        assert!(error.contains("not-a-dir"), "{error}");
    }
}
