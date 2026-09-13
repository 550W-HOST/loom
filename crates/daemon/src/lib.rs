//! The loom host daemon.
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
//! that boundary, with a provider bridge that speaks Pi's RPC protocol. When
//! the Node daemon lands it replaces the bridge, not the contract.
//!
//! # Lifecycle
//!
//! ```text
//!   connect ──▶ welcome ──▶ enroll_host ──▶ host_enrolled ──▶ subscribe host:{id}
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
//! * **Dispatch arrives through the relay.** The daemon subscribes to
//!   `host:{id}` and parses [`RunDispatch`] out of relayed event payloads. It
//!   replays from its cursor on reconnect, so a dispatch published while it was
//!   disconnected is delivered late rather than lost.
//! * **A run always ends.** Every provider process ends in exactly one
//!   `finished` report — see [`provider`] — and the server separately reaps a
//!   run whose deadline passes, so a daemon that dies mid-run cannot leave the
//!   thread `working`.
//!
//! [`RunDispatch`]: loom_provider_protocol::RunDispatch

pub mod acp;
pub mod provider;
pub mod session;
pub mod update;

use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use loom_domain::{HostId, RunId};
use loom_provider_protocol::{
    EnvironmentProvision, EnvironmentProvisionOutcome, EnvironmentProvisionReport, ProviderSpec,
    RunDispatch,
};
use loom_relay::dedup::SeenSet;
use loom_relay::{EventId, Scope};
use loom_server::protocol::{ClientCommand, ServerMessage};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

use crate::provider::ProviderRun;

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// How often a connected daemon reports liveness by default.
pub const DEFAULT_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);

/// How long a provider run may take before the daemon kills it, by default.
pub const DEFAULT_RUN_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// How many dispatch ids the daemon remembers to suppress redelivery.
pub const DISPATCH_DEDUP_CAPACITY: usize = 512;

/// How many reports may be queued before a provider task waits.
const REPORT_CHANNEL_CAPACITY: usize = 256;

/// Where managed environments' workspaces are created by default.
///
/// `LOOM_WORKSPACE_ROOT` overrides it; otherwise `$HOME/.loom/workspaces`, or
/// the system temp directory when there is no home. The daemon owns this
/// layout: the control plane only learns the resulting path from the report.
pub fn default_environment_root() -> PathBuf {
    if let Some(root) = std::env::var_os("LOOM_WORKSPACE_ROOT") {
        return PathBuf::from(root);
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".loom").join("workspaces");
    }
    std::env::temp_dir().join("loom-workspaces")
}

/// A daemon could not connect, could not speak the protocol, or was rejected.
#[derive(Debug)]
pub enum DaemonError {
    /// The socket could not be opened or failed mid-conversation.
    WebSocket(String),
    /// The server sent a frame this daemon could not use.
    Protocol(String),
    /// The server announced a protocol version this build cannot speak.
    ///
    /// Kept apart from the general [`DaemonError::Protocol`] because it is the
    /// one protocol failure with an automatic remedy: the reconnect loop reads
    /// the version out of it and fetches the matching daemon
    /// ([`crate::update`]).
    ProtocolMismatch {
        /// The version the server announced.
        server_protocol_version: u32,
        /// The version this binary speaks.
        local_protocol_version: u32,
    },
}

impl std::fmt::Display for DaemonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DaemonError::WebSocket(message) => write!(f, "websocket: {message}"),
            DaemonError::Protocol(message) => write!(f, "protocol: {message}"),
            DaemonError::ProtocolMismatch {
                server_protocol_version,
                local_protocol_version,
            } => write!(
                f,
                "server speaks protocol version {server_protocol_version}, \
                 this daemon speaks {local_protocol_version}; this daemon must be updated \
                 (upgrade server and daemon together, or let the daemon self-update)"
            ),
        }
    }
}

impl DaemonError {
    /// The protocol version the peer announced, for a version refusal.
    ///
    /// This is the one piece of information the reconnect loop needs out of a
    /// failed connection: it is what the update is requested against, so the
    /// binary fetched is for exactly the version the server announced rather
    /// than for a re-read of it that could have moved.
    pub fn mismatched_protocol_version(&self) -> Option<u32> {
        match self {
            DaemonError::ProtocolMismatch {
                server_protocol_version,
                ..
            } => Some(*server_protocol_version),
            _ => None,
        }
    }
}

impl std::error::Error for DaemonError {}

impl From<tokio_tungstenite::tungstenite::Error> for DaemonError {
    fn from(error: tokio_tungstenite::tungstenite::Error) -> Self {
        DaemonError::WebSocket(error.to_string())
    }
}

impl From<serde_json::Error> for DaemonError {
    fn from(error: serde_json::Error) -> Self {
        DaemonError::Protocol(error.to_string())
    }
}

/// Refuses a server whose welcome protocol version this daemon cannot speak.
///
/// The server and every daemon and UI bundle must agree on
/// [`loom_server::PROTOCOL_VERSION`]: it is the wire contract, not a marketing
/// version. A mixed deployment is rejected here, at the first frame, rather
/// than misbehaving mid-run. See `docs/upgrades.md`.
///
/// A newer server is exactly the case [`crate::update`] handles: the reconnect
/// loop reads the version out of the error, asks the same server for the
/// matching binary, installs it and restarts. Keeping the refusal here —
/// before `enroll`, before a single dispatch — is what makes an update safe to
/// perform: no run is in flight on a connection that never enrolled.
pub fn ensure_compatible_protocol(server_protocol_version: u32) -> Result<(), DaemonError> {
    let local = loom_server::PROTOCOL_VERSION;
    if server_protocol_version == local {
        Ok(())
    } else {
        Err(DaemonError::ProtocolMismatch {
            server_protocol_version,
            local_protocol_version: local,
        })
    }
}

/// Everything a daemon needs to reach and describe itself.
#[derive(Clone, Debug)]
pub struct DaemonConfig {
    /// The server to connect to: `http://host:port`, `https://…`, or an
    /// explicit `ws(s)://` URL. The daemon only ever dials out.
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
    /// Base directory for per-thread provider sessions, when supported.
    pub session_dir: Option<PathBuf>,
    /// Root under which managed environments' workspaces are created.
    ///
    /// A managed environment's directory is `<environment_root>/<env_id>`. The
    /// daemon chooses the actual path and reports it; the control plane never
    /// presumes a layout.
    pub environment_root: PathBuf,
    /// The host-scope event id to resume from. `None` replays the retained
    /// window and relies on dispatch dedup.
    pub resume_cursor: Option<EventId>,
    /// Maximum frames to request in the reconnect replay.
    pub replay_limit: usize,
    /// How the daemon reacts to a server whose protocol does not match.
    ///
    /// [`crate::update::UpdateConfig`] carries "allowed at all", the install
    /// path and the backoff schedule. The reconnect loop is the only party that
    /// uses it, and it is what turns the old hard refusal into "fetch the
    /// matching binary and restart".
    pub update: crate::update::UpdateConfig,
}

impl DaemonConfig {
    /// A configuration with the default heartbeat interval and no provider
    /// override.
    ///
    /// The update configuration defaults to "enabled, install over the running
    /// executable, no persisted state", so a daemon started with no flags at
    /// all still follows a newer server. `UpdateConfig::for_current_binary`
    /// reports the one machine-level failure it can have (a running executable
    /// that cannot be resolved); when it does, self-update is turned off loudly
    /// rather than left half-configured.
    pub fn new(server_url: impl Into<String>, name: impl Into<String>) -> Self {
        let update =
            crate::update::UpdateConfig::for_current_binary(true, None).unwrap_or_else(|error| {
                eprintln!("loom-daemon: self-update disabled: {error}");
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
            session_dir: None,
            environment_root: default_environment_root(),
            resume_cursor: None,
            replay_limit: 500,
            update,
        }
    }

    /// The WebSocket endpoint derived from [`DaemonConfig::server_url`].
    ///
    /// Accepts the URL an operator would paste into a browser and turns it
    /// into the socket path, so "the server is a URL" holds for daemons too.
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
        if base.ends_with("/ws") {
            base
        } else {
            format!("{base}/ws")
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

/// A connected daemon.
///
/// Created by [`Daemon::connect`], identified by [`Daemon::enroll`], kept
/// alive by [`Daemon::run`] or [`Daemon::heartbeat`].
pub struct Daemon {
    socket: Socket,
    config: DaemonConfig,
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
    /// Environment-provisioning reports waiting to be forwarded to the server.
    env_reports: mpsc::Receiver<EnvironmentProvisionReport>,
    env_reports_tx: mpsc::Sender<EnvironmentProvisionReport>,
    /// Runs with a provider task in flight, keyed by run id.
    running: HashSet<RunId>,
}

impl Daemon {
    /// Opens the socket and consumes the server's welcome frame.
    pub async fn connect(config: DaemonConfig) -> Result<Self, DaemonError> {
        let url = config.websocket_url();
        let (mut socket, _) = connect_async(&url).await?;
        match next_message(&mut socket).await? {
            ServerMessage::Welcome {
                protocol_version, ..
            } => {
                // Refuse a peer this build cannot speak to, before enrolling.
                // A mismatch after enrollment would corrupt dispatch/runs.
                ensure_compatible_protocol(protocol_version)?;
                let (reports_tx, reports) = mpsc::channel(REPORT_CHANNEL_CAPACITY);
                let (env_reports_tx, env_reports) = mpsc::channel(REPORT_CHANNEL_CAPACITY);
                Ok(Self {
                    socket,
                    cursor: config.resume_cursor,
                    config,
                    host_id: None,
                    seen_events: SeenSet::new(DISPATCH_DEDUP_CAPACITY),
                    seen_runs: RunSeen::new(DISPATCH_DEDUP_CAPACITY),
                    reports,
                    reports_tx,
                    env_reports,
                    env_reports_tx,
                    running: HashSet::new(),
                })
            }
            other => Err(DaemonError::Protocol(format!(
                "expected welcome, got {other:?}"
            ))),
        }
    }

    /// Enrolls as a host, follows its `host:{id}` room, and replays anything it
    /// missed while disconnected.
    ///
    /// After this returns, the daemon is receiving dispatches: live ones from
    /// the room and anything published while it was away, replayed from the
    /// relay's retained window.
    pub async fn enroll(&mut self) -> Result<HostId, DaemonError> {
        self.send(&ClientCommand::EnrollHost {
            host_id: self.config.host_id.clone(),
            name: self.config.name.clone(),
        })
        .await?;

        let host_id = loop {
            match next_message(&mut self.socket).await? {
                ServerMessage::HostEnrolled { host, .. } => break host.id,
                ServerMessage::Error { message } => {
                    return Err(DaemonError::Protocol(message));
                }
                _ => continue,
            }
        };
        self.host_id = Some(host_id.clone());

        // Follow the room first, then replay: a live dispatch that arrives in
        // between is queued on the socket and also present in the replay
        // window, and the dedup set drops the overlap.
        self.subscribe(Scope::Host(host_id.to_string())).await?;
        self.replay_host_scope(&host_id).await?;
        Ok(host_id)
    }

    /// Sends one heartbeat. Frames are not awaited: heartbeats carry no reply
    /// the execution plane needs, and `run` drains the socket.
    pub async fn heartbeat(&mut self) -> Result<(), DaemonError> {
        let host_id = self
            .host_id
            .clone()
            .ok_or_else(|| DaemonError::Protocol("not enrolled yet".into()))?;
        self.send(&ClientCommand::HostHeartbeat { host_id }).await
    }

    /// Runs until the socket closes: heartbeats on an interval, absorbs
    /// incoming frames, starts providers for dispatches, and forwards the
    /// reports those providers produce.
    ///
    /// Cancelling the future leaves the socket open; call [`Daemon::disconnect`]
    /// to announce the departure before dropping it.
    pub async fn run(&mut self) -> Result<(), DaemonError> {
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
                        None => return Ok(()),
                        Some(message) => self.on_socket_message(message?)?,
                    }
                }
                report = self.reports.recv() => {
                    let Some(report) = report else { continue };
                    if report.event.is_terminal() {
                        self.running.remove(&report.event.run_id);
                    }
                    self.send(&ClientCommand::RunReport {
                        report: Box::new(report),
                    })
                    .await?;
                }
                report = self.env_reports.recv() => {
                    let Some(report) = report else { continue };
                    self.send(&ClientCommand::EnvironmentReport { report }).await?;
                }
            }
        }
    }

    /// Announces the departure and closes the socket.
    pub async fn disconnect(mut self) -> Result<(), DaemonError> {
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
    /// A daemon that restarts can persist this and pass it back as
    /// [`DaemonConfig::resume_cursor`], so it replays exactly what it missed
    /// instead of the whole retained window.
    pub fn cursor(&self) -> Option<&EventId> {
        self.cursor.as_ref()
    }

    /// The run ids with a provider task in flight.
    pub fn running_runs(&self) -> usize {
        self.running.len()
    }

    /// Handles one frame from the server.
    fn on_socket_message(&mut self, message: Message) -> Result<(), DaemonError> {
        match message {
            Message::Text(text) => self.on_server_frame(serde_json::from_str(text.as_str())?),
            Message::Binary(bytes) => self.on_server_frame(serde_json::from_slice(&bytes)?),
            Message::Close(_) => Err(DaemonError::Protocol("server closed the socket".into())),
            Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => Ok(()),
        }
    }

    /// Handles one decoded server message, starting a provider for dispatches.
    fn on_server_frame(&mut self, message: ServerMessage) -> Result<(), DaemonError> {
        match message {
            ServerMessage::Event {
                event_id,
                scope,
                payload,
                ..
            } => self.observe_event(&event_id, &scope, &payload),
            ServerMessage::Error { message } => Err(DaemonError::Protocol(message)),
            // Welcome, acks and pongs are the transport shell's to ignore.
            _ => Ok(()),
        }
    }

    /// Records an event id and starts a run if the payload is a dispatch.
    fn observe_event(
        &mut self,
        event_id: &str,
        scope: &Scope,
        payload: &str,
    ) -> Result<(), DaemonError> {
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

        // Only a dispatch or a provisioning request parses as one; host domain
        // events in the same room (registration, status changes) are neither.
        if let Ok(dispatch) = serde_json::from_str::<RunDispatch>(payload) {
            self.start_dispatch(dispatch);
        } else if let Ok(provision) = serde_json::from_str::<EnvironmentProvision>(payload) {
            self.start_provision(provision);
        }
        Ok(())
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
        // `LOOM_PROVIDER_CMD` from silently running in the daemon's own cwd.
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
            self.config.session_dir.clone(),
        );
        // The launch kind decides which driver runs the turn. They share the
        // report channel and the run identity, so everything above this point
        // — dispatch, reconciliation, the relay — is unaware of the choice.
        match run.spec.launch {
            loom_provider_protocol::ProviderLaunch::JsonRpc => {
                provider::spawn(run, self.reports_tx.clone());
            }
            loom_provider_protocol::ProviderLaunch::AcpStdio => {
                let transport = crate::acp::session::Transport::Stdio {
                    command: run.spec.command.clone(),
                    args: run.spec.args.clone(),
                };
                crate::acp::session::spawn(run, transport, self.reports_tx.clone());
            }
            loom_provider_protocol::ProviderLaunch::AcpEmbeddedPi => {
                crate::acp::session::spawn(
                    run,
                    crate::acp::session::Transport::EmbeddedPi,
                    self.reports_tx.clone(),
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
    async fn subscribe(&mut self, scope: Scope) -> Result<(), DaemonError> {
        self.send(&ClientCommand::Subscribe { scope }).await?;
        loop {
            match next_message(&mut self.socket).await? {
                ServerMessage::Subscribed { .. } => return Ok(()),
                ServerMessage::Error { message } => return Err(DaemonError::Protocol(message)),
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
    async fn replay_host_scope(&mut self, host_id: &HostId) -> Result<(), DaemonError> {
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
                    ServerMessage::Error { message } => return Err(DaemonError::Protocol(message)),
                    _ => continue,
                }
            };

            if !has_more {
                return Ok(());
            }
        }
    }

    async fn send(&mut self, command: &ClientCommand) -> Result<(), DaemonError> {
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

async fn next_message(socket: &mut Socket) -> Result<ServerMessage, DaemonError> {
    loop {
        let message = socket
            .next()
            .await
            .ok_or_else(|| DaemonError::Protocol("server closed the socket".into()))??;
        match message {
            Message::Text(text) => return Ok(serde_json::from_str(text.as_str())?),
            Message::Binary(bytes) => return Ok(serde_json::from_slice(&bytes)?),
            Message::Close(_) => {
                return Err(DaemonError::Protocol("server closed the socket".into()));
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
        let config = DaemonConfig::new("http://127.0.0.1:38886", "laptop");
        assert_eq!(config.websocket_url(), "ws://127.0.0.1:38886/ws");

        let config = DaemonConfig::new("https://loom.example.com/", "laptop");
        assert_eq!(config.websocket_url(), "wss://loom.example.com/ws");

        let config = DaemonConfig::new("ws://host:9/ws", "laptop");
        assert_eq!(config.websocket_url(), "ws://host:9/ws");

        let config = DaemonConfig::new("host:1234", "laptop");
        assert_eq!(config.websocket_url(), "ws://host:1234/ws");
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
