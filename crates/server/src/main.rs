//! `loom-server` binary.
//!
//! This is the **server-only** path. Starting it starts the control plane and
//! nothing else: it never launches a daemon, never waits for one, and does not
//! exit when none is present. A full-stack convenience launcher, if any, is a
//! separate process that supervises this one and a daemon independently.
//!
//! Deliberately thin: parse configuration, wire the state, serve. Every
//! decision worth testing lives in the library.

use loom_domain::HostId;
use loom_server::http::router;
use loom_server::state::{AppConfig, AppState};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Before anything else: `--version` has to answer on a machine with nothing
    // to configure. The server reads every setting from the environment
    // (`deploy/env/loom-server.env`), so this one flag is its whole command
    // line surface, and the release verification runs it before it trusts a
    // downloaded binary.
    if std::env::args().skip(1).any(|arg| arg == "--version") {
        println!("{}", loom_server::version_line("loom-server"));
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
    // connected remote hosts instead of an absent local daemon.
    let local_host_id = match std::env::var("LOOM_LOCAL_HOST_ID") {
        Ok(raw) if !raw.trim().is_empty() => Some(raw.parse::<HostId>()?),
        _ => None,
    };
    // The UI is served from the same origin as the API. By default that is the
    // reference client compiled into this binary; LOOM_UI_DIR points at a built
    // bundle (production) and LOOM_UI_PROXY at a dev server (development).
    let ui_dir = std::env::var_os("LOOM_UI_DIR").map(std::path::PathBuf::from);
    let ui_proxy = match std::env::var("LOOM_UI_PROXY") {
        Ok(url) if !url.trim().is_empty() => Some(url),
        _ => None,
    };

    let config = AppConfig {
        node_id: node_id.clone(),
        backend_path,
        backend_redis,
        local_host_id: local_host_id.clone(),
        ui_dir,
        ui_proxy,
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
            "loom-server (server-only) listening on http://{bind} (node {node_id}, no local daemon)"
        ),
    }
    eprintln!("UI served from {} at http://{bind}/", state.ui.describe());

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
