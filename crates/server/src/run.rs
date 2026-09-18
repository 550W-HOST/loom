//! The **server role**, as a library entry point.
//!
//! Reached through `loom server` (or the `loom-server` name the same binary
//! answers to), and server-only by construction: it never launches a worker,
//! never waits for one, and does not exit when none is present. The other role
//! is a separate process — see `docs/process-model.md`.
//!
//! Deliberately thin: parse configuration, wire the state, serve. Every
//! decision worth testing lives in the rest of this library.

use crate::http::router;
use crate::state::{AppConfig, AppState};
use loom_domain::HostId;

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
    match &local_host_id {
        Some(host_id) => eprintln!(
            "loom-server (server-only) listening on http://{bind} (node {node_id}, local host {host_id})"
        ),
        None => eprintln!(
            "loom-server (server-only) listening on http://{bind} (node {node_id}, no local worker)"
        ),
    }
    eprintln!("UI served from {} at http://{bind}/", state.ui.describe());
    eprintln!("self-update {}", state.artifacts.describe());

    // A SIGINT/Ctrl-C drains connections, then the process writes a final
    // domain snapshot and stops its background tasks. A hard kill skips the
    // snapshot; the periodic writer and log replay are what make that safe.
    let shutdown = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await?;
    state.shutdown();
    Ok(())
}
