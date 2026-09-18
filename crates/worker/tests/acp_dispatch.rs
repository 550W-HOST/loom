//! The ACP launch kind, end to end through a real server and worker.
//!
//! `acp_session.rs` drives the transport directly; this proves the *dispatch*
//! path chooses it: a `ProviderLaunch::AcpStdio` spec travels through the relay,
//! the worker picks the ACP driver for it, and the thread still leaves `working`
//! with exactly one terminal event.
//!
//! Needs a `pi-acp` binary; skips when there is none, like the sibling test.

use std::path::PathBuf;
use std::time::Duration;

use loom_domain::{EnvironmentKind, HostId, HostStatus, MessageRole, ThreadStatus};
use loom_provider_protocol::{ProviderLaunch, ProviderSpec};
use loom_server::http::router;
use loom_server::state::{AppConfig, AppState};
use loom_worker::{Worker, WorkerConfig};
use serde_json::Value;

async fn spawn_server(config: AppConfig) -> (String, AppState) {
    let state = AppState::build(config).unwrap();
    let app = router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{}:{}", addr.ip(), addr.port()), state)
}

fn pi_acp_binary() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("PI_ACP_BIN") {
        let path = PathBuf::from(path);
        return path.is_file().then_some(path);
    }
    for candidate in [
        "../../../pi-acp/target/release/pi-acp",
        "../../../pi-acp/target/debug/pi-acp",
    ] {
        let path = PathBuf::from(candidate);
        if path.is_file() {
            return Some(path);
        }
    }
    None
}

fn acp_spec(binary: &std::path::Path) -> ProviderSpec {
    let mut spec = ProviderSpec::acp(binary.to_string_lossy().into_owned(), Vec::new());
    spec.name = "pi".into();
    spec
}

async fn enroll_worker(
    url: &str,
    provider: Option<ProviderSpec>,
    run_timeout: Duration,
) -> (HostId, tokio::task::JoinHandle<()>) {
    let mut config = WorkerConfig::new(url, "acp-worker");
    config.host_id = None;
    config.provider = provider;
    config.run_timeout = run_timeout;
    config.heartbeat_interval = Duration::from_millis(50);
    let mut worker = Worker::connect(config).await.unwrap();
    let host_id = worker.enroll().await.unwrap();
    let handle = tokio::spawn(async move {
        let _ = worker.run().await;
    });
    (host_id, handle)
}

/// The same shape the ACP end-to-end tests use, so the server↔worker dispatch
/// path and the ACP driver are exercised together.
fn start_turn(
    state: &AppState,
    workspace: &std::path::Path,
    content: &str,
) -> loom_domain::ThreadId {
    let host_id = state
        .registry
        .hosts()
        .into_iter()
        .next()
        .expect("a host must be enrolled before dispatching")
        .id;
    let (environment, _) = state
        .registry
        .create_environment(
            Some(state.registry.personal_project_id()),
            host_id,
            EnvironmentKind::Unmanaged,
            Some(workspace.to_string_lossy().into_owned()),
            loom_relay::now_ms(),
        )
        .unwrap();
    let (thread, created) = state
        .registry
        .create_thread(
            Some(state.registry.personal_project_id()),
            Some("acp thread".into()),
            Some(environment.id),
            loom_relay::now_ms(),
        )
        .unwrap();
    state.publish_domain_event(&created).unwrap();
    let events = state
        .registry
        .post_message(
            &thread.id,
            MessageRole::User,
            content.to_owned(),
            loom_relay::now_ms(),
        )
        .unwrap();
    for event in &events {
        state.publish_domain_event(event).unwrap();
    }
    let thread = state.registry.thread(&thread.id).unwrap();
    assert_eq!(thread.status, ThreadStatus::Working);
    state.dispatch_thread(&thread, content);
    thread.id
}

async fn eventually(mut predicate: impl FnMut() -> bool) -> bool {
    for _ in 0..600 {
        if predicate() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

fn thread_events(state: &AppState, thread_id: &loom_domain::ThreadId) -> Vec<Value> {
    let scope = loom_relay::Scope::Thread(thread_id.to_string());
    state
        .relay
        .replay_scope(&scope, 500)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|envelope| {
            let frame: Value = serde_json::from_slice(&envelope.payload).ok()?;
            let payload = frame["payload"].as_str()?;
            serde_json::from_str(payload).ok()
        })
        .collect()
}

fn turn_completions(events: &[Value]) -> Vec<&Value> {
    events
        .iter()
        .filter(|e| e.pointer("/event/type").and_then(Value::as_str) == Some("turn/completed"))
        .collect()
}

/// A dispatch whose spec says ACP reaches the ACP driver, runs the turn, and
/// ends it once.
#[tokio::test(flavor = "multi_thread")]
async fn an_acp_dispatch_runs_through_a_real_worker() {
    let Some(binary) = pi_acp_binary() else {
        eprintln!("skipping: no pi-acp binary (set PI_ACP_BIN or build the sibling checkout)");
        return;
    };
    let workspace = tempfile::tempdir().unwrap();
    let (url, state) = spawn_server(AppConfig::default()).await;
    let (_host_id, worker) =
        enroll_worker(&url, Some(acp_spec(&binary)), Duration::from_secs(60)).await;

    assert!(
        eventually(|| state
            .registry
            .hosts()
            .iter()
            .any(|h| h.status == HostStatus::Connected))
        .await,
        "the worker connected"
    );

    let thread_id = start_turn(&state, workspace.path(), "hello from the ACP path");

    assert!(
        eventually(|| {
            state
                .registry
                .thread(&thread_id)
                .map(|t| t.status != ThreadStatus::Working)
                .unwrap_or(false)
        })
        .await,
        "the thread left working"
    );

    let events = thread_events(&state, &thread_id);
    let terminals = turn_completions(&events);
    assert_eq!(
        terminals.len(),
        1,
        "exactly one terminal event through the dispatch path: {events:#?}"
    );

    // The lifecycle facts the ACP driver synthesizes, since ACP has no thread
    // or turn concept of its own, must be present in the log.
    let kinds: Vec<_> = events
        .iter()
        .filter_map(|e| e.pointer("/event/type").and_then(Value::as_str))
        .collect();
    assert!(
        kinds.contains(&"thread/identity"),
        "identity is stated: {kinds:?}"
    );
    assert!(
        kinds.contains(&"turn/started"),
        "the turn was opened: {kinds:?}"
    );
    let stored = state.registry.thread(&thread_id).unwrap();
    assert!(
        stored.provider_session_id.is_some(),
        "the server stores the ACP session id learned from thread/identity"
    );

    worker.abort();
}

/// An ACP spec naming a binary that does not exist fails the run rather than
/// leaving the thread in `working`.
#[tokio::test(flavor = "multi_thread")]
async fn an_acp_dispatch_to_a_missing_agent_fails_cleanly() {
    let workspace = tempfile::tempdir().unwrap();
    let (url, state) = spawn_server(AppConfig::default()).await;
    let mut spec = ProviderSpec::acp("/nonexistent/pi-acp".to_string(), Vec::new());
    spec.name = "pi".into();
    let (_host_id, worker) = enroll_worker(&url, Some(spec), Duration::from_secs(10)).await;

    assert!(
        eventually(|| state
            .registry
            .hosts()
            .iter()
            .any(|h| h.status == HostStatus::Connected))
        .await,
        "the worker connected"
    );

    let thread_id = start_turn(&state, workspace.path(), "this will fail");

    assert!(
        eventually(|| {
            state
                .registry
                .thread(&thread_id)
                .map(|t| t.status != ThreadStatus::Working)
                .unwrap_or(false)
        })
        .await,
        "a missing agent still ends the run: {:?}",
        state.registry.thread(&thread_id)
    );

    let events = thread_events(&state, &thread_id);
    assert_eq!(
        turn_completions(&events).len(),
        1,
        "exactly one terminal event: {events:#?}"
    );
    assert_eq!(
        state.registry.thread(&thread_id).unwrap().status,
        ThreadStatus::Error,
        "the thread is in error, not stuck"
    );

    worker.abort();
}

/// The spec serializes the launch kind onto the wire, so a worker on another
/// machine reaches the same driver.
#[test]
fn the_launch_kind_survives_serialization() {
    let spec = ProviderSpec::acp("agent".to_string(), vec!["--x".to_string()]);
    let json = serde_json::to_value(&spec).unwrap();
    assert_eq!(json["launch"], "acp_stdio");

    let back: ProviderSpec = serde_json::from_value(json).unwrap();
    assert_eq!(back.launch, ProviderLaunch::AcpStdio);

    // A dispatch without a launch kind is interpreted as a native ACP agent;
    // The launch kind is explicit: no missing field can re-enable a removed
    // direct provider protocol.
    let legacy: ProviderSpec = serde_json::from_value(serde_json::json!({
        "name": "custom",
        "command": "agent",
        "args": []
    }))
    .unwrap();
    assert_eq!(legacy.launch, ProviderLaunch::AcpStdio);
}
