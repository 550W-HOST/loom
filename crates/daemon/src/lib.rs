//! The loom host daemon.
//!
//! This is the **execution plane as an independent process**. It connects
//! *outbound* to a server URL, enrolls as a host, keeps that host alive with
//! heartbeats, and follows its own `host:{id}` room. It never needs the server
//! to share its machine, process tree, or cgroup, and the server never starts
//! or supervises it.
//!
//! Why this crate exists at all, when the real provider execution will live in
//! bb's Node host-daemon: the process boundary is the deliverable. The wire
//! protocol implemented here (`enroll_host` / `host_heartbeat` /
//! `host_disconnect`) is the contract the Node execution plane implements, and
//! this daemon is the reference implementation plus the test harness that
//! proves a server is usable with and without it.
//!
//! What the daemon deliberately does **not** do:
//!
//! * it does not import a server, a relay, or storage — a daemon that cannot
//!   reach its server is still a well-behaved process;
//! * it does not create or own the server process;
//! * it does not assume the server is local.
//!
//! # Lifecycle
//!
//! ```text
//!   connect ──▶ welcome ──▶ enroll_host ──▶ host_enrolled ──▶ subscribe host:{id}
//!                                                   │
//!                        heartbeat ─────────────────┤ (every interval)
//!                                                   │
//!                        host_disconnect / close ───┘ (on shutdown)
//! ```
//!
//! Stopping the daemon marks the host `disconnected`; the server and every
//! connected UI keep running. That is the whole point of the split.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use loom_domain::HostId;
use loom_relay::Scope;
use loom_server::protocol::{ClientCommand, ServerMessage};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// How often a connected daemon reports liveness by default.
pub const DEFAULT_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);

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
}

impl DaemonConfig {
    /// A configuration with the default heartbeat interval.
    pub fn new(server_url: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            server_url: server_url.into(),
            name: name.into(),
            host_id: None,
            heartbeat_interval: DEFAULT_HEARTBEAT_INTERVAL,
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

/// A connected daemon.
///
/// Created by [`Daemon::connect`], identified by [`Daemon::enroll`], kept
/// alive by [`Daemon::run`] or [`Daemon::heartbeat`].
pub struct Daemon {
    socket: Socket,
    config: DaemonConfig,
    host_id: Option<HostId>,
}

impl Daemon {
    /// Opens the socket and consumes the server's welcome frame.
    pub async fn connect(config: DaemonConfig) -> Result<Self, DaemonError> {
        let url = config.websocket_url();
        let (mut socket, _) = connect_async(&url).await?;
        match next_message(&mut socket).await? {
            ServerMessage::Welcome { .. } => Ok(Self {
                socket,
                config,
                host_id: None,
            }),
            other => Err(DaemonError::Protocol(format!(
                "expected welcome, got {other:?}"
            ))),
        }
    }

    /// Enrolls as a host and follows its `host:{id}` room.
    ///
    /// After this returns, published `host:{id}` frames arrive on the socket.
    pub async fn enroll(&mut self) -> Result<HostId, DaemonError> {
        self.send(&ClientCommand::EnrollHost {
            host_id: self.config.host_id.clone(),
            name: self.config.name.clone(),
        })
        .await?;

        loop {
            match next_message(&mut self.socket).await? {
                ServerMessage::HostEnrolled { host, .. } => {
                    self.host_id = Some(host.id.clone());
                    // Follow our own room so dispatch traffic reaches us. This
                    // is the same subscribe a UI uses; nothing host-specific.
                    self.subscribe(Scope::Host(host.id.to_string())).await?;
                    return Ok(host.id);
                }
                ServerMessage::Error { message } => {
                    return Err(DaemonError::Protocol(message));
                }
                _ => continue,
            }
        }
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
    /// incoming frames, and returns on a clean server-side close.
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
                        Some(message) => match message? {
                            Message::Text(text) => {
                                absorb(serde_json::from_str(text.as_str())?)?;
                            }
                            Message::Binary(bytes) => {
                                absorb(serde_json::from_slice(&bytes)?)?;
                            }
                            Message::Close(_) => return Ok(()),
                            Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => {}
                        },
                    }
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

    async fn send(&mut self, command: &ClientCommand) -> Result<(), DaemonError> {
        let text = serde_json::to_string(command)?;
        self.socket.send(Message::Text(text.into())).await?;
        Ok(())
    }
}

/// Rejects a server frame that indicates the daemon is out of sync.
fn absorb(message: ServerMessage) -> Result<(), DaemonError> {
    match message {
        ServerMessage::Error { message } => Err(DaemonError::Protocol(message)),
        // Events, acks and pongs are the execution plane's to interpret; the
        // transport shell simply keeps the connection healthy.
        _ => Ok(()),
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
}
