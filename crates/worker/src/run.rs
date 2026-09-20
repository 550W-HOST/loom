//! The **worker role**, as a library entry point.
//!
//! Reached through `loom worker`: the worker-only startup path.
//!
//! It reaches out to a server URL and does nothing else. It can run on a
//! different machine from the server, under a different supervisor, and be
//! stopped without touching the control plane:
//!
//! ```bash
//! loom worker --server-url http://127.0.0.1:38886 --name laptop
//! loom worker --server-url https://loom.example.com --name builder-1 \
//!             --state ./builder-1.host-id
//! ```
//!
//! The command line is the whole configuration surface ([`crate::cli`]), except
//! for `--join-code`, which also falls back to `LOOM_JOIN_CODE` because it is a
//! one-time credential.
//!
//! # Lifecycle
//!
//! The binary does not own a connection; it owns a *supervised session*
//! ([`crate::session`]). The loop is: connect, enrol, run, and on a
//! failure reconnect on an exponential backoff. When the server speaks a newer
//! protocol the loop fetches the matching worker from that same server,
//! verifies its SHA-256, installs it with a rename, and **exits** — systemd's
//! `Restart=always` starts the new binary. This process never replaces itself,
//! and never exits into a restart loop that would only be refused again. See
//! `docs/upgrades.md`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::cli::WorkerArgs;
use crate::session::{run_session, SessionOutcome, WorkerState};
use crate::update::{UpdateConfig, Updater};
use crate::WorkerConfig;
use loom_domain::HostId;
use loom_provider_protocol::ProviderSpec;
use loom_relay::EventId;

pub async fn run(args: WorkerArgs) -> Result<(), Box<dyn std::error::Error>> {
    let options = Options::from_args(args)?;

    let mut config = WorkerConfig::new(&options.server_url, &options.name);
    config.heartbeat_interval = options.heartbeat_interval;
    config.run_timeout = options.run_timeout;
    config.permission_timeout = options.permission_timeout;
    if let Some(root) = &options.workspace_root {
        config.environment_root = root.clone();
    }
    if let Some(dir) = &options.data_dir {
        config.data_dir = dir.clone();
    }
    config.provider = options.provider.clone();
    config.join_code = options.join_code.clone();
    config.update = options.update_config()?;

    // The updater is built once. It is absent only when the operator disabled
    // self-update, in which case a protocol mismatch is retried on the normal
    // backoff rather than fetched.
    let updater = if config.update.enabled {
        match Updater::new(config.update.clone(), &options.server_url) {
            Ok(updater) => Some(updater),
            Err(error) => {
                eprintln!("loom-worker: self-update unavailable: {error}");
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
        SessionOutcome::Shutdown => eprintln!("loom-worker \"{}\" stopped", options.name),
        SessionOutcome::RestartForUpdate { detail } => {
            // The exit is the update: systemd's `Restart=always`, or a
            // container's restart policy, is what starts the new binary. A
            // non-zero status here would be recorded as a failure rather than a
            // planned update, so this returns success.
            eprintln!(
                "loom-worker \"{}\" exiting for a self-update: {detail}",
                options.name
            );
        }
    }
    Ok(())
}

/// The worker's machine-local state, under the `--state` path.
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

impl WorkerState for FileState {
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
/// mismatched worker does or does not follow its server is in the journal.
fn describe_update(options: &Options, updater: Option<&Updater>) {
    match (&updater, options.auto_update) {
        (Some(updater), _) => eprintln!(
            "loom-worker self-update: enabled (target {}, installs {} with an exponential \
             backoff of 5s..5m)",
            updater.config().target,
            updater.config().install_path.display()
        ),
        (None, false) => eprintln!(
            "loom-worker self-update: disabled by configuration; a server that speaks a newer \
             protocol will be refused and retried, never fetched"
        ),
        (None, true) => {
            eprintln!("loom-worker self-update: unavailable (see the message above)")
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
    join_code: Option<String>,
    workspace_root: Option<PathBuf>,
    data_dir: Option<PathBuf>,
    state: Option<PathBuf>,
    auto_update: bool,
}

impl Options {
    /// Build the run configuration from the parsed command line.
    fn from_args(args: WorkerArgs) -> Result<Self, String> {
        let WorkerArgs {
            server_url,
            name,
            host_id,
            heartbeat_ms,
            run_timeout_ms,
            permission_timeout_ms,
            state,
            provider_cmd,
            provider_args,
            join_code,
            data_dir,
            workspace_root,
            auto_update: _,
            no_auto_update,
        } = args;

        let server_url = server_url.trim().to_owned();
        if server_url.is_empty() {
            return Err("--server-url must not be empty".into());
        }
        let heartbeat_interval = heartbeat_ms
            .map(Duration::from_millis)
            .unwrap_or(crate::DEFAULT_HEARTBEAT_INTERVAL);
        let run_timeout = run_timeout_ms
            .map(Duration::from_millis)
            .unwrap_or(crate::DEFAULT_RUN_TIMEOUT);
        let permission_timeout = permission_timeout_ms
            .map(Duration::from_millis)
            .unwrap_or(crate::DEFAULT_PERMISSION_TIMEOUT);
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

        Ok(Self {
            server_url,
            name: name.unwrap_or_else(|| crate::cli::DEFAULT_NAME.into()),
            host_id,
            heartbeat_interval,
            run_timeout,
            permission_timeout,
            provider,
            join_code: join_code.filter(|value| !value.trim().is_empty()),
            workspace_root: non_empty(workspace_root),
            data_dir: non_empty(data_dir),
            state: non_empty(state),
            // `--no-auto-update` is the only way to turn it off. The default is
            // on because a worker that cannot follow a server upgrade is the
            // operational trap this exists to remove; `--auto-update` is kept
            // so a unit can spell the default out.
            auto_update: !no_auto_update,
        })
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

/// `--data-dir ""` is a missing directory, not the current directory.
fn non_empty(path: Option<PathBuf>) -> Option<PathBuf> {
    path.filter(|value| !value.as_os_str().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse a worker command line the way the dispatcher does.
    fn options(args: &[&str]) -> Options {
        use clap::Parser;
        let mut argv = vec!["loom".to_owned()];
        argv.extend(args.iter().map(|arg| (*arg).to_owned()));
        let parsed = WorkerArgs::try_parse_from(argv).expect("the worker command line parses");
        Options::from_args(parsed).expect("the options are consistent")
    }

    #[test]
    fn self_update_is_on_by_default_and_the_flag_turns_it_off() {
        assert!(options(&["--server-url", "http://x:1"]).auto_update);
        assert!(options(&["--server-url", "http://x:1", "--auto-update"]).auto_update);
        assert!(!options(&["--server-url", "http://x:1", "--no-auto-update"]).auto_update);
        assert!(!options(&["--server-url", "http://x:1", "--disable-auto-update"]).auto_update);
    }

    #[test]
    fn join_code_is_read_from_the_command_line() {
        assert_eq!(
            options(&["--server-url", "http://x:1", "--join-code", "loom-code"])
                .join_code
                .as_deref(),
            Some("loom-code")
        );
    }

    #[test]
    fn a_missing_server_url_is_refused_before_anything_else() {
        use clap::Parser;
        let error = WorkerArgs::try_parse_from(["loom"]).expect_err("--server-url is required");
        assert_eq!(
            error.kind(),
            clap::error::ErrorKind::MissingRequiredArgument
        );
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
