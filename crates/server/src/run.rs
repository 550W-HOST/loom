//! The **server role**, as a library entry point.
//!
//! Reached through `loom server` (or the `loom-server` name the same binary
//! answers to). Server-only by default: it never launches a worker, never waits
//! for one, and does not exit when none is present. The other role is a
//! separate process — see `docs/process-model.md`.
//!
//! The one exception is opt-in and named: `--local-worker` starts **one**
//! `loom worker` child on this machine and supervises it, which is the
//! single-box convenience. It is a child process with its own address space and
//! reconnect loop, not an embedded execution plane; `crate::local_worker` says
//! what is and is not shared.
//!
//! Deliberately thin: take the parsed command line, wire the state, serve. Every
//! decision worth testing lives in the rest of this library, and the command
//! line itself is [`crate::cli`].

use crate::cli::ServerArgs;
use crate::http::router;
use crate::local_worker::{server_url_for_bind, LocalWorker, LocalWorkerConfig};
use crate::state::{AppConfig, AppState};

pub async fn run(args: ServerArgs) -> Result<(), Box<dyn std::error::Error>> {
    let ServerArgs {
        bind,
        data_dir,
        node_id,
        redis_url,
        local_host_id,
        ui_proxy,
        artifact_dir,
        local_worker,
        local_worker_name,
        local_worker_url,
    } = args;

    // Without --data-dir the relay log is in-process and the server needs no
    // configuration at all. Setting it turns on the durable backend.
    let backend_path = data_dir;
    // --redis-url moves the log into Redis Streams so several nodes share one
    // window and a server restart does not lose it. It replaces, rather than
    // supplements, the local data directory.
    let backend_redis = match redis_url.as_deref().map(str::trim) {
        Some(url) if !url.is_empty() => {
            Some(loom_relay::backend::redis::RedisConfig::from_url(url)?)
        }
        _ => None,
    };
    // The UI is the product app compiled into this binary, so a server serves a
    // client with no configuration at all. --ui-proxy is the one override, for
    // developing the app against a real server.

    // The single-box convenience, resolved before `AppConfig` takes ownership
    // of the data path. The child gets its own data directory under the same
    // root: sharing one with the server would put the relay log and a host-id
    // file in the same place by accident.
    let local_worker_config = if local_worker {
        let server_url = match local_worker_url {
            Some(url) => url,
            None => server_url_for_bind(bind),
        };
        let worker_data_dir = backend_path.as_ref().map(|root| root.join("local-worker"));
        Some(LocalWorkerConfig {
            server_url,
            name: local_worker_name.clone(),
            state_path: worker_data_dir.as_ref().map(|dir| dir.join("host-id")),
            data_dir: worker_data_dir,
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

    let listener = tokio::net::TcpListener::bind(bind).await?;

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
