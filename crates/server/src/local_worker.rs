//! `--local-worker`: the server's opt-in supervisor for one worker child.
//!
//! `loom server` is server-only by construction, and that is still the default:
//! no flag, no worker process, multi-machine shapes unchanged. This module is
//! the single-machine convenience — the operator asks for it, and the server
//! starts and supervises *one* worker child on the same box.
//!
//! It is a supervisor, not an embedding. The child is a real `loom worker`
//! process: its own address space, its own provider and tool children, its own
//! reconnect loop. The server's only new responsibility is to
//!
//! 1. start that child once the listener is up,
//! 2. restart it when it exits — including the `exit 0` a worker self-update
//!    leaves behind, which is why a supervisor is required
//!    (`docs/upgrades.md`), and
//! 3. kill it when the server stops, so a stopped control plane does not leave
//!    a worker reconnecting at a socket nobody serves.
//!
//! What it deliberately does not do: there is no FFI, no Node runtime and no
//! shared memory. Cutting the child off is `kill`, not a protocol shutdown, and
//! the worker on the other side is exactly the worker a second machine runs.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::process::{Child, Command};
use tokio::sync::watch;

/// The first restart delay after a local worker exits.
const INITIAL_RESTART_DELAY: Duration = Duration::from_secs(1);
/// The ceiling the delay backs off to. A worker that cannot stay up should not
/// spin a core; its transient failures (a server restart, an update) recover on
/// their own within a few seconds.
const MAX_RESTART_DELAY: Duration = Duration::from_secs(30);

/// Everything the child needs, resolved by the caller before it is started.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalWorkerConfig {
    /// The server the child dials. The child only makes outbound connections.
    pub server_url: String,
    /// Display name in the host list.
    pub name: String,
    /// The child's data directory. `None` leaves the worker's own default
    /// (`$HOME/.loom`) in place, which is what a server with no `--data-dir`
    /// has to do.
    pub data_dir: Option<PathBuf>,
    /// File the enrolled host id is persisted in. `None` uses the worker's own
    /// default beside its data directory.
    pub state_path: Option<PathBuf>,
    /// The child's silence budget for one provider run, when the server chose
    /// one. `None` leaves the worker's own default in place.
    ///
    /// The server reaps a run the worker is still nursing if its own ceiling is
    /// the lower of the two, so a server that was given budgets hands the same
    /// values to the child it starts rather than letting the two drift.
    pub run_timeout_ms: Option<u64>,
    /// The child's total budget for one provider run, when the server chose
    /// one. `None` leaves the worker's own default in place.
    pub run_ceiling_ms: Option<u64>,
}

/// The URL a co-located worker dials for a given `--bind`.
///
/// A bind on the unspecified address (`0.0.0.0`, `[::]`) is reachable on
/// loopback; a bind on one specific address is reachable only there, so the
/// same address is what the child is given. Getting this wrong is not a crash:
/// it is a worker that retries forever against a port nothing listens on, which
/// is why it is a function with tests rather than a `format!` at the call site.
pub fn server_url_for_bind(address: std::net::SocketAddr) -> String {
    let host = match address.ip() {
        std::net::IpAddr::V4(ip) if ip.is_unspecified() => "127.0.0.1".to_owned(),
        std::net::IpAddr::V4(ip) => ip.to_string(),
        std::net::IpAddr::V6(ip) if ip.is_unspecified() => "127.0.0.1".to_owned(),
        std::net::IpAddr::V6(ip) => format!("[{ip}]"),
    };
    format!("http://{host}:{}", address.port())
}

/// The program to run in the worker role.
///
/// A sibling `loom-worker` is preferred when one exists (an operator may keep
/// one, and anything else that spawns a worker by name uses it). Otherwise this
/// executable is used, with the `worker` subcommand: one installed `loom` is
/// both roles, so the local worker needs no second name.
pub fn worker_program() -> Result<PathBuf, String> {
    let current = std::env::current_exe()
        .map_err(|error| format!("cannot resolve this executable: {error}"))?;
    if let Some(sibling) = worker_sibling(&current) {
        if sibling.is_file() {
            return Ok(sibling);
        }
    }
    Ok(current)
}

/// `…/loom` → `…/loom-worker`, the sibling name the installer links.
pub fn worker_sibling(current: &Path) -> Option<PathBuf> {
    let directory = current.parent()?;
    let name = if cfg!(windows) {
        "loom-worker.exe"
    } else {
        "loom-worker"
    };
    Some(directory.join(name))
}

/// A started supervisor. Dropping it without `shutdown` leaves the task running,
/// so callers hold it for the server's whole lifetime.
pub struct LocalWorker {
    shutdown: watch::Sender<bool>,
    handle: tokio::task::JoinHandle<()>,
}

impl LocalWorker {
    /// Start supervising a worker child.
    ///
    /// The program and the child's data directory are resolved here rather than
    /// inside the task so a bad `LOOM_WORKER_BIN` or an unwritable path is a
    /// startup error instead of a silent crash-restart loop. The child itself is
    /// spawned by the task, so a server that is up before its worker is the
    /// normal case rather than a startup race.
    pub fn start(config: LocalWorkerConfig) -> Result<Self, String> {
        let program = worker_program()?;
        // The worker persists its host id with a plain write-and-rename, which
        // does not create parents: a missing directory would leave the child
        // connected but identity-less, and every restart would enroll a new
        // machine. Create it here, once, where a failure is still fatal.
        if let Some(directory) = &config.data_dir {
            std::fs::create_dir_all(directory)
                .map_err(|error| format!("cannot create {}: {error}", directory.display()))?;
        }
        let (shutdown, receiver) = watch::channel(false);
        let handle = tokio::spawn(supervise(program, config, receiver));
        Ok(Self { shutdown, handle })
    }

    /// Ask the supervisor to stop and wait for it to reap the child.
    pub async fn shutdown(self) {
        let _ = self.shutdown.send(true);
        let _ = self.handle.await;
    }
}

async fn supervise(
    program: PathBuf,
    config: LocalWorkerConfig,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut delay = INITIAL_RESTART_DELAY;
    loop {
        if *shutdown.borrow() {
            return;
        }
        match spawn_worker(&program, &config) {
            Ok(mut child) => {
                delay = INITIAL_RESTART_DELAY;
                tokio::select! {
                    status = child.wait() => {
                        if *shutdown.borrow() {
                            return;
                        }
                        match status {
                            Ok(status) => eprintln!(
                                "loom-server: local worker exited ({status}); \
                                 restarting in {}s",
                                delay.as_secs()
                            ),
                            Err(error) => eprintln!(
                                "loom-server: local worker could not be waited on \
                                 ({error}); restarting in {}s",
                                delay.as_secs()
                            ),
                        }
                    }
                    _ = shutdown.changed() => {
                        let _ = child.start_kill();
                        let _ = child.wait().await;
                        return;
                    }
                }
            }
            Err(error) => {
                if *shutdown.borrow() {
                    return;
                }
                eprintln!(
                    "loom-server: local worker could not start: {error}; \
                     retrying in {}s",
                    delay.as_secs()
                );
            }
        }
        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            _ = shutdown.changed() => return,
        }
        delay = (delay * 2).min(MAX_RESTART_DELAY);
    }
}

/// Build and spawn one worker child.
///
/// Everything the child needs is a flag: the worker role reads no environment
/// variable as configuration, so there is nothing to override or clear here.
fn spawn_worker(program: &Path, config: &LocalWorkerConfig) -> std::io::Result<Child> {
    let mut command = Command::new(program);
    command
        .args(worker_argv(config))
        .stdin(Stdio::null())
        // A server killed hard still takes its worker with it as far as the
        // kernel can arrange: this covers the graceful path, and the systemd
        // unit's `KillMode=control-group` covers the rest.
        .kill_on_drop(true);
    command.spawn()
}

/// The child's arguments, minus the program name.
///
/// Everything the child needs is a flag. The run budgets ride along when the
/// server was given them: the two processes measure the same run, and a child
/// left on a longer ceiling than its server would be killed by the server's
/// reaper instead of its own watchdog.
fn worker_argv(config: &LocalWorkerConfig) -> Vec<std::ffi::OsString> {
    let mut args: Vec<std::ffi::OsString> = vec![
        "worker".into(),
        "--server-url".into(),
        config.server_url.clone().into(),
        "--name".into(),
        config.name.clone().into(),
    ];
    if let Some(path) = &config.state_path {
        args.push("--state".into());
        args.push(path.clone().into());
    }
    if let Some(directory) = &config.data_dir {
        args.push("--data-dir".into());
        args.push(directory.clone().into());
    }
    if let Some(ms) = config.run_timeout_ms {
        args.push("--run-timeout-ms".into());
        args.push(ms.to_string().into());
    }
    if let Some(ms) = config.run_ceiling_ms {
        args.push("--run-ceiling-ms".into());
        args.push(ms.to_string().into());
    }
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    fn address(bind: &str) -> std::net::SocketAddr {
        bind.parse().expect("a socket address")
    }

    #[test]
    fn a_loopback_bind_becomes_a_loopback_url() {
        assert_eq!(
            server_url_for_bind(address("127.0.0.1:38886")),
            "http://127.0.0.1:38886"
        );
    }

    #[test]
    fn an_unspecified_bind_is_dialled_on_loopback() {
        assert_eq!(
            server_url_for_bind(address("0.0.0.0:38886")),
            "http://127.0.0.1:38886"
        );
        assert_eq!(
            server_url_for_bind(address("[::]:38886")),
            "http://127.0.0.1:38886"
        );
    }

    #[test]
    fn a_specific_bind_is_dialled_where_it_is_bound() {
        assert_eq!(
            server_url_for_bind(address("192.168.1.5:9000")),
            "http://192.168.1.5:9000"
        );
        assert_eq!(
            server_url_for_bind(address("[::1]:9000")),
            "http://[::1]:9000"
        );
    }

    #[test]
    fn the_sibling_worker_name_sits_beside_this_executable() {
        let sibling = worker_sibling(Path::new("/usr/local/bin/loom")).unwrap();
        assert_eq!(sibling, PathBuf::from("/usr/local/bin/loom-worker"));
    }

    fn config() -> LocalWorkerConfig {
        LocalWorkerConfig {
            server_url: "http://127.0.0.1:38886".into(),
            name: "local".into(),
            data_dir: None,
            state_path: None,
            run_timeout_ms: None,
            run_ceiling_ms: None,
        }
    }

    fn value_of<'a>(argv: &'a [std::ffi::OsString], flag: &str) -> Option<&'a str> {
        argv.iter()
            .position(|arg| arg == flag)
            .and_then(|index| argv.get(index + 1))
            .and_then(|value| value.to_str())
    }

    /// The child only hears about the run budgets the server was given.
    #[test]
    fn the_child_inherits_the_servers_run_budgets() {
        let plain = worker_argv(&config());
        assert_eq!(value_of(&plain, "--run-timeout-ms"), None);
        assert_eq!(value_of(&plain, "--run-ceiling-ms"), None);

        let bounded = LocalWorkerConfig {
            run_timeout_ms: Some(1_500),
            run_ceiling_ms: Some(9_000),
            ..config()
        };
        let argv = worker_argv(&bounded);
        assert_eq!(value_of(&argv, "--run-timeout-ms"), Some("1500"));
        assert_eq!(value_of(&argv, "--run-ceiling-ms"), Some("9000"));
        // The budgets are additive: the rest of the command line is unchanged.
        assert_eq!(
            value_of(&argv, "--server-url"),
            Some("http://127.0.0.1:38886")
        );
        assert_eq!(value_of(&argv, "--name"), Some("local"));
    }
}
