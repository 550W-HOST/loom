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
//! `LOOM_HOST_NAME`, `LOOM_HOST_ID`, `LOOM_HEARTBEAT_MS`, `LOOM_DAEMON_STATE`,
//! `LOOM_PROVIDER_CMD`, `LOOM_PROVIDER_ARGS`,
//! `LOOM_RUN_TIMEOUT_MS`, `LOOM_WORKSPACE_ROOT`, `LOOM_AUTO_UPDATE`) so a
//! systemd unit needs no command line.
//!
//! # Lifecycle
//!
//! The binary does not own a connection; it owns a *supervised session*
//! ([`loom_daemon::session`]). The loop is: connect, enrol, run, and on a
//! failure reconnect on an exponential backoff. When the server speaks a newer
//! protocol the loop fetches the matching daemon from that same server,
//! verifies its SHA-256, installs it with a rename, and **exits** — systemd's
//! `Restart=always` starts the new binary. This process never replaces itself,
//! and never exits into a restart loop that would only be refused again. See
//! `docs/upgrades.md`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use loom_daemon::session::{run_session, DaemonState, SessionOutcome};
use loom_daemon::update::{UpdateConfig, Updater};
use loom_daemon::DaemonConfig;
use loom_domain::HostId;
use loom_provider_protocol::ProviderSpec;
use loom_relay::EventId;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Ahead of `Options::parse`, which refuses a command line with no
    // `--server-url`: `--version` must answer on a machine that has not been
    // pointed at a server yet. That is also the check a release verification
    // runs against a downloaded daemon, before it tries to connect it to
    // anything.
    if std::env::args().skip(1).any(|arg| arg == "--version") {
        println!("{}", loom_server::version_line("loom-daemon"));
        return Ok(());
    }

    let options = match Options::parse(std::env::args().skip(1))? {
        Some(options) => options,
        None => {
            print_help();
            return Ok(());
        }
    };

    let mut config = DaemonConfig::new(&options.server_url, &options.name);
    config.heartbeat_interval = options.heartbeat_interval;
    config.run_timeout = options.run_timeout;
    config.permission_timeout = options.permission_timeout;
    if let Some(root) = &options.workspace_root {
        config.environment_root = root.clone();
    }
    config.provider = options.provider.clone();
    config.update = options.update_config()?;

    // The updater is built once. It is absent only when the operator disabled
    // self-update, in which case a protocol mismatch is retried on the normal
    // backoff rather than fetched.
    let updater = if config.update.enabled {
        match Updater::new(config.update.clone(), &options.server_url) {
            Ok(updater) => Some(updater),
            Err(error) => {
                eprintln!("loom-daemon: self-update unavailable: {error}");
                config.update.enabled = false;
                None
            }
        }
    } else {
        None
    };
    describe_update(&options, updater.as_ref());

    let state = FileState {
        path: options.state.clone(),
        explicit_host_id: options.host_id.clone(),
    };
    let outcome = run_session(config, updater.as_ref(), &state, shutdown_signal()).await;

    match outcome {
        SessionOutcome::Shutdown => eprintln!("loom-daemon \"{}\" stopped", options.name),
        SessionOutcome::RestartForUpdate { detail } => {
            // The exit is the update: systemd's `Restart=always`, or a
            // container's restart policy, is what starts the new binary. A
            // non-zero status here would be recorded as a failure rather than a
            // planned update, so this returns success.
            eprintln!(
                "loom-daemon \"{}\" exiting for a self-update: {detail}",
                options.name
            );
        }
    }
    Ok(())
}

/// The daemon's machine-local state, in the files `deploy/install.sh` lays out.
///
/// One struct knows the on-disk layout: `<state>` holds the host id and a
/// sibling `.cursor` holds the replay position, both written atomically so a
/// crash cannot leave a truncated identity behind. This is the same
/// crash-safety idea the update attempt counter uses.
struct FileState {
    path: Option<PathBuf>,
    /// A host id given on the command line, which wins over the file until the
    /// first enrollment writes it there. This is how a machine is migrated to a
    /// new identity without hand-editing the state file.
    explicit_host_id: Option<HostId>,
}

impl FileState {
    fn cursor_path(&self) -> Option<PathBuf> {
        self.path
            .as_deref()
            .map(|path| Path::new(path).with_extension("cursor"))
    }
}

impl DaemonState for FileState {
    fn host_id(&self) -> Result<Option<HostId>, String> {
        if let Some(host_id) = &self.explicit_host_id {
            return Ok(Some(host_id.clone()));
        }
        read_optional(self.path.as_deref(), "host id")
    }

    fn save_host_id(&self, host_id: &HostId) -> Result<(), String> {
        write_atomic(self.path.as_deref(), &format!("{host_id}\n"))
    }

    fn cursor(&self) -> Result<Option<EventId>, String> {
        read_optional(self.cursor_path().as_deref(), "cursor")
    }

    fn save_cursor(&self, cursor: Option<&EventId>) -> Result<(), String> {
        let (Some(cursor), Some(_)) = (cursor, self.cursor_path()) else {
            return Ok(());
        };
        write_atomic(self.cursor_path().as_deref(), &format!("{cursor}\n"))
    }
}

/// Reads and parses a persisted value. A missing file is not an error: it is
/// the first run.
fn read_optional<T: std::str::FromStr<Err = E>, E: std::fmt::Display>(
    path: Option<&Path>,
    what: &str,
) -> Result<Option<T>, String> {
    let Some(path) = path else {
        return Ok(None);
    };
    let Ok(raw) = std::fs::read_to_string(path) else {
        return Ok(None);
    };
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(None);
    }
    raw.parse::<T>()
        .map(Some)
        .map_err(|error| format!("{}: not a valid {what}: {error}", path.display()))
}

/// Writes beside the destination and renames, so a crash mid-write leaves the
/// previous value rather than a partial one.
fn write_atomic(path: Option<&Path>, contents: &str) -> Result<(), String> {
    let Some(path) = path else {
        return Ok(());
    };
    let temporary = path.with_extension(format!("tmp.{}", std::process::id()));
    std::fs::write(&temporary, contents)
        .map_err(|error| format!("{}: {error}", temporary.display()))?;
    std::fs::rename(&temporary, path).map_err(|error| {
        let _ = std::fs::remove_file(&temporary);
        format!("{}: {error}", path.display())
    })
}

/// Ctrl-C, as a future the session loop selects against.
async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

/// Logs the self-update configuration once, at startup, so the reason a
/// mismatched daemon does or does not follow its server is in the journal.
fn describe_update(options: &Options, updater: Option<&Updater>) {
    match (&updater, options.auto_update) {
        (Some(updater), _) => eprintln!(
            "loom-daemon self-update: enabled (target {}, installs {} with an exponential \
             backoff of 5s..5m)",
            updater.config().target,
            updater.config().install_path.display()
        ),
        (None, false) => eprintln!(
            "loom-daemon self-update: disabled by configuration; a server that speaks a newer \
             protocol will be refused and retried, never fetched"
        ),
        (None, true) => {
            eprintln!("loom-daemon self-update: unavailable (see the message above)")
        }
    }
}

/// Parsed command line, with environment fallbacks.
struct Options {
    server_url: String,
    name: String,
    host_id: Option<HostId>,
    heartbeat_interval: Duration,
    run_timeout: Duration,
    permission_timeout: Duration,
    provider: Option<ProviderSpec>,
    workspace_root: Option<PathBuf>,
    state: Option<PathBuf>,
    auto_update: bool,
}

impl Options {
    /// Returns `None` for `--help`.
    fn parse(args: impl Iterator<Item = String>) -> Result<Option<Self>, String> {
        let mut server_url = std::env::var("LOOM_SERVER_URL").ok();
        let mut name = std::env::var("LOOM_HOST_NAME").ok();
        let mut host_id = std::env::var("LOOM_HOST_ID").ok();
        let mut heartbeat_ms = std::env::var("LOOM_HEARTBEAT_MS").ok();
        let mut run_timeout_ms = std::env::var("LOOM_RUN_TIMEOUT_MS").ok();
        let mut permission_timeout_ms = std::env::var("LOOM_PERMISSION_TIMEOUT_MS").ok();
        let mut state = std::env::var("LOOM_DAEMON_STATE").ok();
        let mut provider_cmd = std::env::var("LOOM_PROVIDER_CMD").ok();
        let mut provider_args = std::env::var("LOOM_PROVIDER_ARGS").ok();
        let mut workspace_root = std::env::var("LOOM_WORKSPACE_ROOT").ok();
        // `--auto-update` is the affirmative of bb's flag: loom's default is on,
        // because a daemon that cannot follow a server upgrade is the
        // operational trap this exists to remove. `LOOM_AUTO_UPDATE=0` (or any
        // of `false`/`no`/`off`) disables it, and so does the flag below.
        let mut auto_update = parse_bool_env("LOOM_AUTO_UPDATE")?.unwrap_or(true);

        let mut args = args.peekable();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "-h" | "--help" => return Ok(None),
                "--server-url" => server_url = args.next(),
                "--name" => name = args.next(),
                "--host-id" => {
                    host_id = args.next();
                }
                "--heartbeat-ms" => heartbeat_ms = args.next(),
                "--run-timeout-ms" => run_timeout_ms = args.next(),
                "--permission-timeout-ms" => permission_timeout_ms = args.next(),
                "--state" => state = args.next(),
                "--provider-cmd" => provider_cmd = args.next(),
                "--provider-args" => provider_args = args.next(),
                "--workspace-root" => workspace_root = args.next(),
                // bb spells the switch `--auto-update`; loom keeps the spelling
                // and defaults it on. Both flags are accepted so a unit written
                // for either spelling works, and the disabled reason is logged.
                "--auto-update" => auto_update = true,
                "--no-auto-update" | "--disable-auto-update" => auto_update = false,
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
        let run_timeout = match run_timeout_ms {
            None => loom_daemon::DEFAULT_RUN_TIMEOUT,
            Some(raw) => Duration::from_millis(
                raw.parse::<u64>()
                    .map_err(|error| format!("--run-timeout-ms: {error}"))?,
            ),
        };
        let permission_timeout = match permission_timeout_ms {
            None => loom_daemon::DEFAULT_PERMISSION_TIMEOUT,
            Some(raw) => Duration::from_millis(
                raw.parse::<u64>()
                    .map_err(|error| format!("--permission-timeout-ms: {error}"))?,
            ),
        };
        // Only build an override when the operator actually chose one; an
        // unset command means "run whatever the control plane dispatched".
        let provider = provider_cmd
            .filter(|value| !value.trim().is_empty())
            .map(|command| {
                let args = provider_args
                    .as_deref()
                    .unwrap_or("")
                    .split_whitespace()
                    .map(str::to_owned)
                    .collect();
                ProviderSpec::acp(command, args)
            });

        Ok(Some(Self {
            server_url,
            name,
            host_id,
            heartbeat_interval,
            run_timeout,
            permission_timeout,
            provider,
            workspace_root: workspace_root
                .filter(|value| !value.trim().is_empty())
                .map(PathBuf::from),
            state: state.map(PathBuf::from),
            auto_update,
        }))
    }

    /// Builds the updater's configuration from the parsed options.
    fn update_config(&self) -> Result<UpdateConfig, String> {
        let state_dir = self
            .state
            .as_deref()
            .and_then(Path::parent)
            .map(Path::to_path_buf);
        UpdateConfig::for_current_binary(self.auto_update, state_dir)
    }
}

/// Reads a boolean environment variable, accepting the spellings a systemd
/// environment file or a compose file realistically uses.
fn parse_bool_env(name: &str) -> Result<Option<bool>, String> {
    let Ok(raw) = std::env::var(name) else {
        return Ok(None);
    };
    match raw.trim().to_ascii_lowercase().as_str() {
        "" => Ok(None),
        "1" | "true" | "yes" | "on" => Ok(Some(true)),
        "0" | "false" | "no" | "off" => Ok(Some(false)),
        other => Err(format!("{name}: expected a boolean, got {other:?}")),
    }
}

#[allow(clippy::needless_pass_by_value)]
fn print_help() {
    println!(
        "loom-daemon — connect this machine to a loom server as an execution host

USAGE:
    loom-daemon --server-url <URL> [--name <NAME>] [--host-id <HOST_ID>]
                [--heartbeat-ms <MS>] [--run-timeout-ms <MS>]
                [--permission-timeout-ms <MS>]
                [--provider-cmd <CMD>] [--provider-args <ARGS>]
                [--state <PATH>]
                [--auto-update | --no-auto-update]

FLAGS:
    --server-url <URL>       Server to dial out to. Required.
                             Env: LOOM_SERVER_URL
    --name <NAME>            Display name for this machine. Default: loom-daemon.
                             Env: LOOM_HOST_NAME
    --host-id <HOST_ID>      Reuse an enrolled identity across restarts. Written
                             to --state on first connect. Env: LOOM_HOST_ID
    --heartbeat-ms <MS>      Liveness interval. Default: 15000.
                             Env: LOOM_HEARTBEAT_MS
    --run-timeout-ms <MS>    Kill a provider that has not settled by then.
                             Default: 1800000. Env: LOOM_RUN_TIMEOUT_MS
    --permission-timeout-ms <MS>
                             Cancel an agent's permission request that no client
                             answered by then. A cancellation is never an
                             approval. Default: 300000.
                             Env: LOOM_PERMISSION_TIMEOUT_MS
    --provider-cmd <CMD>     Override the ACP agent executable. Default: the
                             provider named in the dispatch (Pi uses embedded
                             pi-acp).
                             Env: LOOM_PROVIDER_CMD
    --provider-args <ARGS>   Space-separated arguments for the ACP agent
                             override. Env: LOOM_PROVIDER_ARGS
    --workspace-root <PATH>  Root under which managed environments' workspaces
                             are created as <root>/<env_id>. Default:
                             $HOME/.loom/workspaces. Env: LOOM_WORKSPACE_ROOT
    --state <PATH>           File to persist the enrolled host id in. A sibling
                             `.cursor` file persists the replay cursor, and the
                             directory also holds the self-update attempt
                             counter. Env: LOOM_DAEMON_STATE
    --auto-update            Follow a server that speaks a newer protocol by
                             installing that server's own daemon and exiting for
                             the supervisor to restart. Default.
    --no-auto-update         Refuse to self-update; the refusal and the reason
                             are logged, and the connection is retried. Env:
                             LOOM_AUTO_UPDATE=0
    --version                Print the version, target triple, protocol
                             version and commit, then exit.
    -h, --help               Print this help.

The daemon only makes outbound connections; it needs no local server and is
stopped independently of one. After a successful self-update it exits and
`Restart=always` (or a container restart policy) starts the new binary."
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(args: &[&str]) -> Options {
        // Environment fallbacks are already resolved by the caller in
        // production; tests pass everything explicitly.
        Options::parse(args.iter().map(|arg| (*arg).to_owned()))
            .unwrap()
            .unwrap()
    }

    #[test]
    fn self_update_is_on_by_default_and_the_flag_turns_it_off() {
        assert!(options(&["--server-url", "http://x:1"]).auto_update);
        assert!(options(&["--server-url", "http://x:1", "--auto-update"]).auto_update);
        assert!(!options(&["--server-url", "http://x:1", "--no-auto-update"]).auto_update);
        assert!(!options(&["--server-url", "http://x:1", "--disable-auto-update"]).auto_update);
    }

    #[test]
    fn a_boolean_environment_value_rejects_nonsense() {
        // The variable name is deliberately one no other test sets.
        std::env::remove_var("LOOM_TEST_BOOL_UNSET");
        assert_eq!(parse_bool_env("LOOM_TEST_BOOL_UNSET").unwrap(), None);
        std::env::set_var("LOOM_TEST_BOOL_UNSET", "off");
        assert_eq!(parse_bool_env("LOOM_TEST_BOOL_UNSET").unwrap(), Some(false));
        std::env::set_var("LOOM_TEST_BOOL_UNSET", "TRUE");
        assert_eq!(parse_bool_env("LOOM_TEST_BOOL_UNSET").unwrap(), Some(true));
        std::env::set_var("LOOM_TEST_BOOL_UNSET", "maybe");
        assert!(parse_bool_env("LOOM_TEST_BOOL_UNSET").is_err());
        std::env::remove_var("LOOM_TEST_BOOL_UNSET");
    }

    #[test]
    fn the_state_file_round_trips_and_tolerates_absence() {
        let dir = tempfile::tempdir().unwrap();
        let state = FileState {
            path: Some(dir.path().join("host-id")),
            explicit_host_id: None,
        };
        assert_eq!(state.host_id().unwrap(), None);
        assert_eq!(state.cursor().unwrap(), None);

        let host_id = HostId::mint();
        state.save_host_id(&host_id).unwrap();
        assert_eq!(state.host_id().unwrap(), Some(host_id));

        let cursor = "01ARZ3NDEKTSV4RRFFQ69G5FAV"
            .parse::<EventId>()
            .expect("a well-formed event id");
        state.save_cursor(Some(&cursor)).unwrap();
        assert_eq!(state.cursor().unwrap(), Some(cursor));
        assert!(state.cursor_path().unwrap().exists());

        // Saving an absent cursor is a no-op, not an error.
        state.save_cursor(None).unwrap();
        assert_eq!(state.cursor().unwrap(), Some(cursor));
    }

    #[test]
    fn a_corrupted_state_file_is_reported_not_silently_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("host-id");
        std::fs::write(&path, "not-a-host-id\n").unwrap();
        let state = FileState {
            path: Some(path),
            explicit_host_id: None,
        };
        let error = state.host_id().unwrap_err();
        assert!(error.contains("host id"), "{error}");
    }

    #[test]
    fn an_explicit_host_id_wins_over_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let persisted = HostId::mint();
        let path = dir.path().join("host-id");
        std::fs::write(&path, format!("{persisted}\n")).unwrap();

        let explicit = HostId::mint();
        let state = FileState {
            path: Some(path),
            explicit_host_id: Some(explicit.clone()),
        };
        assert_eq!(state.host_id().unwrap(), Some(explicit));
    }
}
