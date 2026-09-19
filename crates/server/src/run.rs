//! The **server role**, as a library entry point.
//!
//! Reached through `loom server` (or the `loom-server` name the same binary
//! answers to). Server-only by default: it never launches a worker, never waits
//! for one, and does not exit when none is present. The other role is a
//! separate process — see `docs/process-model.md`.
//!
//! The one exception is opt-in and named: `--local-worker` (or
//! `LOOM_LOCAL_WORKER=1`) starts **one** `loom worker` child on this machine and
//! supervises it, which is the single-box convenience. It is a child process
//! with its own address space and reconnect loop, not an embedded execution
//! plane; `crate::local_worker` says what is and is not shared.
//!
//! Deliberately thin: parse configuration, wire the state, serve. Every
//! decision worth testing lives in the rest of this library.

use crate::http::router;
use crate::local_worker::{server_url_for_bind, LocalWorker, LocalWorkerConfig};
use crate::state::{AppConfig, AppState};
use loom_domain::HostId;

/// The `--local-worker` surface: on/off plus the two things an operator may want
/// to override (the name in the host list, and the URL the child dials).
#[derive(Clone, Debug, Default)]
struct LocalWorkerFlags {
    enabled: bool,
    name: Option<String>,
    url: Option<String>,
}

impl LocalWorkerFlags {
    fn parse(args: &[String]) -> Result<Self, String> {
        let mut flags = Self {
            enabled: parse_bool_env("LOOM_LOCAL_WORKER")?.unwrap_or(false),
            name: std::env::var("LOOM_LOCAL_WORKER_NAME").ok(),
            url: std::env::var("LOOM_LOCAL_WORKER_URL").ok(),
        };
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--local-worker" => flags.enabled = true,
                "--local-worker-name" => flags.name = args.next().cloned(),
                "--local-worker-url" => flags.url = args.next().cloned(),
                other => return Err(format!("unrecognised argument: {other}")),
            }
        }
        // `--local-worker-name ""` is a missing name, not a host called "".
        flags.name = flags.name.filter(|value| !value.trim().is_empty());
        flags.url = flags.url.filter(|value| !value.trim().is_empty());
        Ok(flags)
    }
}

/// `LOOM_LOCAL_WORKER=0` must mean off, not unset, so this is a real parse.
fn parse_bool_env(name: &str) -> Result<Option<bool>, String> {
    let Ok(raw) = std::env::var(name) else {
        return Ok(None);
    };
    match raw.trim().to_ascii_lowercase().as_str() {
        "" => Ok(None),
        "1" | "true" | "yes" | "on" => Ok(Some(true)),
        "0" | "false" | "no" | "off" => Ok(Some(false)),
        other => Err(format!("{name}: expected a boolean, got \"{other}\"")),
    }
}

pub async fn run(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    // Before anything else: `--version` has to answer on a machine with nothing
    // to configure. The server reads every setting from the environment
    // (`deploy/env/loom-server.env`), so this one flag is its whole command
    // line surface, and the release verification runs it before it trusts a
    // downloaded binary.
    if args.iter().any(|arg| arg == "--version") {
        println!("{}", crate::version_line("loom-server"));
        return Ok(());
    }

    let local_worker_flags = LocalWorkerFlags::parse(args)?;

    let bind = std::env::var("LOOM_BIND").unwrap_or_else(|_| "127.0.0.1:38886".into());
    let node_id = std::env::var("LOOM_NODE_ID").unwrap_or_else(|_| "loom-node".into());
    // Without LOOM_DATA_DIR the relay log is in-process and the server needs
    // no configuration at all. Setting it turns on the durable backend.
    let backend_path = std::env::var_os("LOOM_DATA_DIR").map(std::path::PathBuf::from);
    // LOOM_REDIS_URL moves the log into Redis Streams so several nodes share
    // one window and a server restart does not lose it. It replaces, rather
    // than supplements, the local data directory.
    let backend_redis = match std::env::var("LOOM_REDIS_URL") {
        Ok(url) if !url.trim().is_empty() => Some(
            loom_relay::backend::redis::RedisConfig::from_url(url.trim())?,
        ),
        _ => None,
    };
    // Optional: declare which enrolled host runs on this machine. Unset (the
    // default) is the server-only shape — primary-host queries fall to
    // connected remote hosts instead of an absent local worker.
    let local_host_id = match std::env::var("LOOM_LOCAL_HOST_ID") {
        Ok(raw) if !raw.trim().is_empty() => Some(raw.parse::<HostId>()?),
        _ => None,
    };
    // The UI is the product app compiled into this binary, so a server serves
    // a client with no configuration at all. LOOM_UI_PROXY is the one override,
    // for developing the app against a real server.
    //
    // `LOOM_UI_DIR` is not read any more. An environment file that still sets
    // it is not an error — the variable is simply inert, and saying so once at
    // startup is the whole of this server's opinion about it. Refusing to boot
    // over a leftover line would turn a stale config into an outage.
    if std::env::var_os("LOOM_UI_DIR").is_some() {
        eprintln!(
            "loom-server: ignoring LOOM_UI_DIR: the product app is compiled into this binary, \
             so there is no bundle path to configure"
        );
    }
    let ui_proxy = match std::env::var("LOOM_UI_PROXY") {
        Ok(url) if !url.trim().is_empty() => Some(url),
        _ => None,
    };
    // Where the worker binaries this server hosts live. Unset (the default)
    // falls back to the directory holding this executable, which is exactly
    // where `deploy/install.sh` puts the matching `loom-worker`, so a default
    // deployment hosts its own artifacts with no configuration.
    let artifact_dir = std::env::var_os("LOOM_ARTIFACT_DIR").map(std::path::PathBuf::from);

    // The single-box convenience, resolved before `AppConfig` takes ownership
    // of the data path. The child gets its own data directory under the same
    // root: sharing one with the server would put the relay log and a host-id
    // file in the same place by accident.
    let local_worker_config = if local_worker_flags.enabled {
        let server_url = match local_worker_flags.url.clone() {
            Some(url) => url,
            None => server_url_for_bind(&bind)?,
        };
        let data_dir = backend_path.as_ref().map(|root| root.join("local-worker"));
        Some(LocalWorkerConfig {
            server_url,
            name: local_worker_flags
                .name
                .clone()
                .unwrap_or_else(|| "loom-local".into()),
            state_path: data_dir.as_ref().map(|dir| dir.join("host-id")),
            data_dir,
        })
    } else {
        None
    };

    let config = AppConfig {
        node_id: node_id.clone(),
        backend_path,
        backend_redis,
        local_host_id: local_host_id.clone(),
        ui_proxy,
        artifact_dir,
        ..AppConfig::default()
    };
    let state = AppState::build(config)?;
    let app = router(state.clone());

    let listener = tokio::net::TcpListener::bind(&bind).await?;

    // Started after the listener exists, so the child's first connection has
    // something to land on. It retries anyway: a worker that cannot reach the
    // server yet is a worker that reconnects, not a startup failure.
    let local_worker = match &local_worker_config {
        Some(worker) => Some(
            LocalWorker::start(worker.clone())
                .map_err(|error| format!("--local-worker: {error}"))?,
        ),
        None => None,
    };

    match (&local_host_id, &local_worker_config) {
        (_, Some(worker)) => eprintln!(
            "loom-server (server + local worker \"{}\") listening on http://{bind} \
             (node {node_id})",
            worker.name
        ),
        (Some(host_id), None) => eprintln!(
            "loom-server (server-only) listening on http://{bind} (node {node_id}, local host {host_id})"
        ),
        (None, None) => eprintln!(
            "loom-server (server-only) listening on http://{bind} (node {node_id}, no local worker)"
        ),
    }
    eprintln!("UI served from {} at http://{bind}/", state.ui.describe());
    eprintln!("self-update {}", state.artifacts.describe());

    // A SIGINT/Ctrl-C or SIGTERM drains connections, then the process writes a
    // final domain snapshot and stops its background tasks. A hard kill skips
    // the snapshot; the periodic writer and log replay are what make that safe.
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    // The child is a different process with a different lifetime, so stopping
    // the server stops it explicitly rather than by inheritance. Doing it before
    // `state.shutdown` keeps the worker from talking while the log is closing.
    if let Some(local_worker) = local_worker {
        local_worker.shutdown().await;
    }
    state.shutdown();
    Ok(())
}

/// Resolve when the process is asked to stop: Ctrl-C, or SIGTERM.
///
/// SIGTERM is what systemd sends, so handling it is what makes the unit's
/// `TimeoutStopSec` grace period and the final snapshot real rather than
/// theoretical. A platform without SIGTERM (or a process that cannot install
/// the handler) falls back to Ctrl-C alone instead of failing to serve.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut terminate) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = terminate.recv() => {}
                }
            }
            Err(error) => {
                eprintln!("loom-server: cannot listen for SIGTERM ({error}); Ctrl-C only");
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
