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
/// The fallback display name if the platform does not expose a host name.
pub const DEFAULT_LOCAL_WORKER_NAME: &str = "loom-local";

/// Resolve the local worker's default display name from the machine it runs on.
pub fn default_local_worker_name() -> String {
    hostname::get()
        .ok()
        .map(|name| name.to_string_lossy().trim().to_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| DEFAULT_LOCAL_WORKER_NAME.to_owned())
}

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

    /// Where the server keeps its data. Unset uses the default directory
    /// (`$HOME/.loom/server`).
    ///
    /// This chooses *where*, never *whether*: a server is persistent by
    /// default, and the default directory is not an optimisation anyone has to
    /// discover. A path that cannot be created or written fails startup rather
    /// than quietly running without persistence.
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
    #[arg(
        long,
        value_name = "NAME",
        default_value_t = crate::cli::default_local_worker_name()
    )]
    pub local_worker_name: String,

    /// URL the local worker dials. Unset derives it from --bind.
    #[arg(long, value_name = "URL")]
    pub local_worker_url: Option<String>,

    /// Reap a dispatched run that has said nothing for this long.
    ///
    /// This is the control plane's half of the worker's `--run-timeout-ms`: it
    /// bounds silence, not the turn. It must not be shorter than the worker's
    /// own bound, or this side reaps runs the worker is still nursing;
    /// `--local-worker` passes these values to the child so the two cannot
    /// drift. 0 removes the bound. [default: 1800000]
    #[arg(long, value_name = "MS")]
    pub run_timeout_ms: Option<u64>,

    /// Reap a dispatched run that has taken this long in total, however active
    /// it is. Keep it at least the worker's own `--run-ceiling-ms`, for the same
    /// reason. 0 removes the bound. [default: 21600000]
    #[arg(long, value_name = "MS")]
    pub run_ceiling_ms: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn an_omitted_local_worker_name_uses_the_machine_hostname() {
        let args = ServerArgs::try_parse_from(["loom"]).expect("server arguments parse");
        assert_eq!(args.local_worker_name, default_local_worker_name());
    }

    #[test]
    fn an_explicit_local_worker_name_is_preserved() {
        let args = ServerArgs::try_parse_from(["loom", "--local-worker-name", "local-test"])
            .expect("server arguments parse");
        assert_eq!(args.local_worker_name, "local-test");
    }

    /// Unset means "the worker's own default", not "the smallest value".
    #[test]
    fn the_run_budgets_are_only_set_when_the_operator_chooses_them() {
        let args = ServerArgs::try_parse_from(["loom"]).expect("server arguments parse");
        assert_eq!(args.run_timeout_ms, None);
        assert_eq!(args.run_ceiling_ms, None);

        let args = ServerArgs::try_parse_from([
            "loom",
            "--run-timeout-ms",
            "1500",
            "--run-ceiling-ms",
            "9000",
        ])
        .expect("server arguments parse");
        assert_eq!(args.run_timeout_ms, Some(1_500));
        assert_eq!(args.run_ceiling_ms, Some(9_000));
    }
}
