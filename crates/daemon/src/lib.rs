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

pub mod provider;

use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use loom_domain::{HostId, RunId};
use loom_provider_protocol::{ProviderSpec, RunDispatch};
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

/// A daemon could not connect, could not speak the protocol, or was rejected.
#[derive(Debug)]
pub enum DaemonError {
    /// The socket could not be opened or failed mid-conversation.
    WebSocket(String),
    /// The server sent a frame this daemon could not use.
    Protocol(String),
}

impl std::fmt::Display for DaemonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DaemonError::WebSocket(message) => write!(f, "websocket: {message}"),
            DaemonError::Protocol(message) => write!(f, "protocol: {message}"),
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
pub fn ensure_compatible_protocol(server_protocol_version: u32) -> Result<(), DaemonError> {
    let local = loom_server::PROTOCOL_VERSION;
    if server_protocol_version == local {
        Ok(())
    } else {
        Err(DaemonError::Protocol(format!(
            "server speaks protocol version {server_protocol_version}, \
             this daemon speaks {local}; upgrade server and daemon together"
        )))
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
    /// The host-scope event id to resume from. `None` replays the retained
    /// window and relies on dispatch dedup.
    pub resume_cursor: Option<EventId>,
    /// Maximum frames to request in the reconnect replay.
    pub replay_limit: usize,
}

impl DaemonConfig {
    /// A configuration with the default heartbeat interval and no provider
    /// override.
    pub fn new(server_url: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            server_url: server_url.into(),
            name: name.into(),
            host_id: None,
            heartbeat_interval: DEFAULT_HEARTBEAT_INTERVAL,
            provider: None,
            run_timeout: DEFAULT_RUN_TIMEOUT,
            session_dir: None,
            resume_cursor: None,
            replay_limit: 500,
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
                Ok(Self {
                    socket,
                    cursor: config.resume_cursor,
                    config,
                    host_id: None,
                    seen_events: SeenSet::new(DISPATCH_DEDUP_CAPACITY),
                    seen_runs: RunSeen::new(DISPATCH_DEDUP_CAPACITY),
                    reports,
                    reports_tx,
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
                        self.running.remove(&report.run_id);
                    }
                    self.send(&ClientCommand::RunReport { report }).await?;
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
            if self.cursor.as_ref().map_or(true, |cursor| id > cursor) {
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

        // Only a dispatch parses as one; host domain events in the same room
        // (registration, status changes) are not dispatches and are ignored.
        let Ok(dispatch) = serde_json::from_str::<RunDispatch>(payload) else {
            return Ok(());
        };
        self.start_dispatch(dispatch);
        Ok(())
    }

    /// Starts a provider for a dispatch unless it was already started.
    fn start_dispatch(&mut self, dispatch: RunDispatch) {
        if self.seen_runs.contains(&dispatch.run_id) || self.running.contains(&dispatch.run_id) {
            return;
        }
        self.seen_runs.insert(&dispatch.run_id);
        self.running.insert(dispatch.run_id.clone());

        let spec = self
            .config
            .provider
            .clone()
            .unwrap_or_else(|| dispatch.provider.clone());
        let run = ProviderRun::from_dispatch(
            &dispatch,
            spec,
            self.config.run_timeout,
            self.config.session_dir.clone(),
        );
        provider::spawn(run, self.reports_tx.clone());
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
}
