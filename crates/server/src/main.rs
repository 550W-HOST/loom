//! `loom-server` binary.
//!
//! Deliberately thin: parse configuration, wire the state, serve. Every
//! decision worth testing lives in the library.

use loom_server::http::router;
use loom_server::state::{AppConfig, AppState};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let bind = std::env::var("LOOM_BIND").unwrap_or_else(|_| "127.0.0.1:38886".into());
    let node_id = std::env::var("LOOM_NODE_ID").unwrap_or_else(|_| "loom-node".into());
    // Without LOOM_DATA_DIR the relay log is in-process and the server needs
    // no configuration at all. Setting it turns on the durable backend.
    let backend_path = std::env::var_os("LOOM_DATA_DIR").map(std::path::PathBuf::from);

    let config = AppConfig {
        node_id: node_id.clone(),
        backend_path,
        ..AppConfig::default()
    };
    let state = AppState::build(config)?;
    let app = router(state.clone());

    let listener = tokio::net::TcpListener::bind(&bind).await?;
    eprintln!("loom-server listening on http://{bind} (node {node_id})");

    axum::serve(listener, app).await?;
    state.shutdown();
    Ok(())
}
