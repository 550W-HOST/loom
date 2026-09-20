//! The server role's command line.
//!
//! Flags are the configuration surface. The one exception carries an `env`
//! fallback because it is a value a command line is a bad home for: a one-time
//! join code, marked `hide_env_values` so `--help` cannot print it.
//!
//! `--version` is answered by the dispatcher (`crates/loom/src/main.rs`) before
//! clap runs, because the release contract fixes its exact shape
//! ([`crate::build_info::version_line`](crate::build_info)); clap's own version
//! flag is therefore disabled here.

use std::net::SocketAddr;
use std::path::PathBuf;

use clap::Parser;
use loom_domain::HostId;

/// The default listen address: loopback, because the API has no authentication.
pub const DEFAULT_BIND: &str = "127.0.0.1:38886";
/// The default node identity stamped on every envelope this node produces.
pub const DEFAULT_NODE_ID: &str = "loom-node";
/// The default display name of the machine's local worker.
pub const DEFAULT_LOCAL_WORKER_NAME: &str = "loom-local";

/// `loom server` — the control plane.
#[derive(Debug, Clone, Parser)]
#[command(
    name = "loom server",
    about = "The loom control plane: the HTTP API, the WebSocket surfaces and the in-process relay.",
    disable_version_flag = true
)]
pub struct ServerArgs {
    /// Address to listen on. Keep it on loopback: the API has no
    /// authentication and can drive command execution on every enrolled host.
    #[arg(long, value_name = "ADDR", default_value = DEFAULT_BIND)]
    pub bind: SocketAddr,

    /// Directory for the durable relay log. Unset keeps the log in memory and
    /// makes the server stateless across restarts.
    #[arg(long, value_name = "PATH")]
    pub data_dir: Option<PathBuf>,

    /// Identity stamped on every envelope this node produces. Unique per node
    /// when more than one server shares a log.
    #[arg(long, value_name = "ID", default_value = DEFAULT_NODE_ID)]
    pub node_id: String,

    /// Removed. A shared Redis log was withdrawn with multi-server support.
    ///
    /// The flag is kept as a tombstone so a deployment that still passes it —
    /// or still exports `LOOM_REDIS_URL` — fails at startup with a reason,
    /// instead of silently starting an unshared in-memory log.
    #[arg(
        long,
        value_name = "URL",
        env = "LOOM_REDIS_URL",
        hide_env_values = true
    )]
    pub redis_url: Option<String>,

    /// The host id of the worker on this machine, so primary-host queries
    /// prefer it while that worker is attached.
    #[arg(long, value_name = "HOST_ID")]
    pub local_host_id: Option<HostId>,

    /// Reverse-proxy the UI to this dev server instead of serving the app
    /// compiled into the binary. Development only.
    #[arg(long, value_name = "URL")]
    pub ui_proxy: Option<String>,

    /// Directory the server hosts worker artifacts from. Unset uses the
    /// directory holding this executable.
    #[arg(long, value_name = "PATH")]
    pub artifact_dir: Option<PathBuf>,

    /// Also start and supervise one `loom worker` on this machine — the
    /// single-box shape. The child is a separate process.
    #[arg(long)]
    pub local_worker: bool,

    /// Display name of the local worker in the host list.
    #[arg(long, value_name = "NAME", default_value = DEFAULT_LOCAL_WORKER_NAME)]
    pub local_worker_name: String,

    /// URL the local worker dials. Unset derives it from --bind.
    #[arg(long, value_name = "URL")]
    pub local_worker_url: Option<String>,
}
