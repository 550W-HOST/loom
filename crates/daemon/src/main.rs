//! `loom-daemon` binary — the **daemon-only** startup path.
//!
//! It reaches out to a server URL and does nothing else. It can run on a
//! different machine from the server, under a different supervisor, and be
//! stopped without touching the control plane:
//!
//! ```bash
//! loom-daemon --server-url http://127.0.0.1:38886 --name laptop
//! loom-daemon --server-url https://loom.example.com --name builder-1 \
//!             --state ./builder-1.host-id
//! ```
//!
//! Everything is also settable through the environment (`LOOM_SERVER_URL`,
//! `LOOM_HOST_NAME`, `LOOM_HOST_ID`, `LOOM_HEARTBEAT_MS`, `LOOM_DAEMON_STATE`)
//! so a systemd unit needs no command line.

use std::path::PathBuf;
use std::time::Duration;

use loom_daemon::{Daemon, DaemonConfig};
use loom_domain::HostId;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let options = match Options::parse(std::env::args().skip(1))? {
        Some(options) => options,
        None => {
            print_help();
            return Ok(());
        }
    };

    let mut host_id = options.host_id.clone();
    if host_id.is_none() {
        host_id = options.read_persisted_host_id()?;
    }

    let mut config = DaemonConfig::new(&options.server_url, &options.name);
    config.host_id = host_id;
    config.heartbeat_interval = options.heartbeat_interval;

    let mut daemon = Daemon::connect(config).await?;
    let enrolled = daemon.enroll().await?;
    eprintln!(
        "loom-daemon \"{}\" enrolled as {enrolled} with {}",
        options.name, options.server_url
    );
    options.persist_host_id(&enrolled)?;

    tokio::select! {
        result = daemon.run() => {
            result?;
            eprintln!("loom-daemon \"{}\" lost its server connection", options.name);
        }
        _ = tokio::signal::ctrl_c() => {
            eprintln!("loom-daemon \"{}\" stopping", options.name);
            daemon.disconnect().await?;
        }
    }
    Ok(())
}

/// Parsed command line, with environment fallbacks.
struct Options {
    server_url: String,
    name: String,
    host_id: Option<HostId>,
    heartbeat_interval: Duration,
    state: Option<PathBuf>,
}

impl Options {
    /// Returns `None` for `--help`.
    fn parse(args: impl Iterator<Item = String>) -> Result<Option<Self>, String> {
        let mut server_url = std::env::var("LOOM_SERVER_URL").ok();
        let mut name = std::env::var("LOOM_HOST_NAME").ok();
        let mut host_id = std::env::var("LOOM_HOST_ID").ok();
        let mut heartbeat_ms = std::env::var("LOOM_HEARTBEAT_MS").ok();
        let mut state = std::env::var("LOOM_DAEMON_STATE").ok();

        let mut args = args.peekable();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "-h" | "--help" => return Ok(None),
                "--server-url" => server_url = args.next(),
                "--name" => name = args.next(),
                "--host-id" => host_id = args.next(),
                "--heartbeat-ms" => heartbeat_ms = args.next(),
                "--state" => state = args.next(),
                other => return Err(format!("unrecognised argument: {other}")),
            }
        }

        let server_url = server_url
            .filter(|value| !value.trim().is_empty())
            .ok_or("--server-url (or LOOM_SERVER_URL) is required")?;
        let name = name.unwrap_or_else(|| "loom-daemon".into());
        let host_id = match host_id.filter(|value| !value.trim().is_empty()) {
            None => None,
            Some(raw) => Some(raw.parse::<HostId>().map_err(|error| error.to_string())?),
        };
        let heartbeat_interval = match heartbeat_ms {
            None => loom_daemon::DEFAULT_HEARTBEAT_INTERVAL,
            Some(raw) => Duration::from_millis(
                raw.parse::<u64>()
                    .map_err(|error| format!("--heartbeat-ms: {error}"))?,
            ),
        };

        Ok(Some(Self {
            server_url,
            name,
            host_id,
            heartbeat_interval,
            state: state.map(PathBuf::from),
        }))
    }

    /// Reads a previously enrolled host id, so a restart keeps the machine's
    /// identity instead of registering a second host. A missing file is not an
    /// error: it is the first run.
    fn read_persisted_host_id(&self) -> Result<Option<HostId>, String> {
        let Some(path) = &self.state else {
            return Ok(None);
        };
        let Ok(raw) = std::fs::read_to_string(path) else {
            return Ok(None);
        };
        let raw = raw.trim();
        if raw.is_empty() {
            return Ok(None);
        }
        raw.parse::<HostId>()
            .map(Some)
            .map_err(|error| format!("{}: {error}", path.display()))
    }

    fn persist_host_id(&self, host_id: &HostId) -> Result<(), String> {
        let Some(path) = &self.state else {
            return Ok(());
        };
        std::fs::write(path, format!("{host_id}\n"))
            .map_err(|error| format!("{}: {error}", path.display()))
    }
}

fn print_help() {
    println!(
        "loom-daemon — connect this machine to a loom server as an execution host

USAGE:
    loom-daemon --server-url <URL> [--name <NAME>] [--host-id <HOST_ID>]
                [--heartbeat-ms <MS>] [--state <PATH>]

FLAGS:
    --server-url <URL>     Server to dial out to. Required.
                           Env: LOOM_SERVER_URL
    --name <NAME>          Display name for this machine. Default: loom-daemon.
                           Env: LOOM_HOST_NAME
    --host-id <HOST_ID>    Reuse an enrolled identity across restarts.
                           Env: LOOM_HOST_ID
    --heartbeat-ms <MS>    Liveness interval. Default: 15000.
                           Env: LOOM_HEARTBEAT_MS
    --state <PATH>         File to persist the enrolled host id in. Optional.
                           Env: LOOM_DAEMON_STATE
    -h, --help             Print this help.

The daemon only makes outbound connections; it needs no local server and is
stopped independently of one."
    );
}
