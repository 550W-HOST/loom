//! The worker role's command line.
//!
//! Flags are the configuration surface; the one `env` fallback is `--join-code`,
//! a one-time enrollment capability that a command line is a poor home for. It
//! is marked `hide_env_values` so `--help` cannot print it.
//!
//! `--version` is answered by the dispatcher (`crates/loom/src/main.rs`) before
//! clap runs, so clap's own version flag is disabled here.

use std::path::PathBuf;

use clap::Parser;
use loom_domain::HostId;

/// The default display name of a worker that does not name itself.
pub const DEFAULT_NAME: &str = "loom-worker";

/// `loom worker` — the execution plane on one machine.
#[derive(Debug, Clone, Parser)]
#[command(
    name = "loom worker",
    about = "Connect this machine to a loom server as an execution host.",
    disable_version_flag = true
)]
pub struct WorkerArgs {
    /// Server to dial out to, as a browser would open it. The worker only makes
    /// outbound connections.
    #[arg(long, value_name = "URL")]
    pub server_url: String,

    /// Display name for this machine in the host list. Keep it stable.
    #[arg(long, value_name = "NAME")]
    pub name: Option<String>,

    /// Reuse an enrolled identity across restarts; written to --state on first
    /// connect.
    #[arg(long, value_name = "HOST_ID")]
    pub host_id: Option<HostId>,

    /// Liveness heartbeat interval. Must stay well below the server's staleness
    /// threshold. [default: 15000]
    #[arg(long, value_name = "MS")]
    pub heartbeat_ms: Option<u64>,

    /// Kill a provider run that has not settled by then. [default: 1800000]
    #[arg(long, value_name = "MS")]
    pub run_timeout_ms: Option<u64>,

    /// Give up on an accepted turn that has said nothing at all for this long.
    /// This is the embedded pi-acp's own settle fallback: it bounds silence,
    /// not the turn, so a long command or a long answer is not silence (a tool
    /// the agent is running holds it off). 0 leaves --run-timeout-ms as the
    /// only bound. [default: 600000]
    #[arg(long, value_name = "MS")]
    pub settle_timeout_ms: Option<u64>,

    /// Cancel an agent's permission request that no client answered by then. A
    /// cancellation is never an approval. [default: 300000]
    #[arg(long, value_name = "MS")]
    pub permission_timeout_ms: Option<u64>,

    /// File to persist the enrolled host id in; a sibling `.cursor` file holds
    /// the replay position. Without it every restart enrolls a new machine.
    #[arg(long, value_name = "PATH")]
    pub state: Option<PathBuf>,

    /// Override the ACP agent executable the dispatch names. Unset runs
    /// whatever the control plane dispatched.
    #[arg(long, value_name = "CMD")]
    pub provider_cmd: Option<String>,

    /// Space-separated arguments for the --provider-cmd override.
    #[arg(long, value_name = "ARGS")]
    pub provider_args: Option<String>,

    /// One-time code from POST /api/v1/hosts/join-codes, for first enrollment.
    /// Remove it after the first successful enrollment. Falls back to
    /// LOOM_JOIN_CODE because it is a credential.
    #[arg(
        long,
        value_name = "CODE",
        env = "LOOM_JOIN_CODE",
        hide_env_values = true
    )]
    pub join_code: Option<String>,

    /// This machine's data directory. Thread storage lives under it and the
    /// server names it from what this worker reports at enrollment.
    /// [default: $HOME/.loom]
    #[arg(long, value_name = "PATH")]
    pub data_dir: Option<PathBuf>,

    /// Root under which managed environments' workspaces are created as
    /// <root>/<env_id>. [default: $HOME/.loom/workspaces]
    #[arg(long, value_name = "PATH")]
    pub workspace_root: Option<PathBuf>,

    /// Follow a server that speaks a newer protocol by installing that server's
    /// own worker and exiting for the supervisor to restart. This is the
    /// default; the flag exists so a unit can spell it out.
    #[arg(long, overrides_with = "no_auto_update")]
    pub auto_update: bool,

    /// Refuse to self-update; the refusal and its reason are logged and the
    /// connection is retried. The binary is never replaced.
    #[arg(
        long = "no-auto-update",
        visible_alias = "disable-auto-update",
        overrides_with = "auto_update"
    )]
    pub no_auto_update: bool,
}
