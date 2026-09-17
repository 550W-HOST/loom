//! Provider execution end to end, over real sockets and real processes.
//!
//! Each test starts a real server, enrolls a real daemon, and runs a real
//! provider process — a small stub that speaks ACP over JSON-RPC. The ACP
//! client and translator are exercised directly in the crate's unit tests; here
//! the point is the path around them: dispatch through the relay, provider
//! output reported back, and the thread leaving `working` with exactly one
//! terminal event, including when the provider crashes, hangs or the daemon
//! disappears.

use std::path::Path;
use std::time::Duration;

use loom_daemon::{Daemon, DaemonConfig};
use loom_domain::{
    EnvironmentKind, EnvironmentStatus, HostId, HostStatus, MessageRole, ThreadId, ThreadStatus,
};
use loom_provider_protocol::ProviderSpec;
use loom_server::http::router;
use loom_server::state::{AppConfig, AppState};
use loom_server::PROTOCOL_VERSION;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Starts a server-only control plane on an ephemeral port.
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

/// Writes an executable ACP agent stub and returns its spec.
///
/// The supplied body runs for `session/prompt`. Keeping the JSON-RPC server
/// shell here means every end-to-end test exercises the real ACP client rather
/// than the removed direct-Pi JSONL driver.
fn write_stub(dir: &Path, name: &str, prompt_body: &str) -> ProviderSpec {
    let path = dir.join(name);
    let script = format!(
        r#"#!/bin/sh
session_id=stub-session
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([^,]*\),"method":.*/\1/p')
  method=$(printf '%s' "$line" | sed -n 's/.*"method":"\([^"]*\)".*/\1/p')
  case "$method" in
    initialize)
      printf '{{"jsonrpc":"2.0","id":%s,"result":{{"protocolVersion":1,"agentCapabilities":{{"loadSession":true}}}}}}\n' "$id"
      ;;
    session/new)
      printf '{{"jsonrpc":"2.0","id":%s,"result":{{"sessionId":"%s"}}}}\n' "$id" "$session_id"
      ;;
    session/load)
      touch "$0.loaded"
      printf '{{"jsonrpc":"2.0","id":%s,"result":{{}}}}\n' "$id"
      ;;
    session/prompt)
      {prompt_body}
      printf '{{"jsonrpc":"2.0","id":%s,"result":{{"stopReason":"end_turn"}}}}\n' "$id"
      ;;
    session/cancel)
      printf '{{"jsonrpc":"2.0","id":%s,"result":{{}}}}\n' "$id"
      ;;
    *)
      printf '{{"jsonrpc":"2.0","id":%s,"result":{{}}}}\n' "$id"
      ;;
  esac
done
"#,
        prompt_body = prompt_body
    );
    std::fs::write(&path, script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&path, permissions).unwrap();
    }
    ProviderSpec::acp(path.to_string_lossy().into_owned(), Vec::new())
}

/// Connects and enrolls a daemon, then drives it on a background task.
async fn enroll_daemon(
    url: &str,
    host_id: Option<HostId>,
    provider: Option<ProviderSpec>,
    run_timeout: Duration,
) -> (HostId, tokio::task::JoinHandle<()>) {
    let mut config = DaemonConfig::new(url, "test-daemon");
    config.host_id = host_id;
    config.provider = provider;
    config.run_timeout = run_timeout;
    config.heartbeat_interval = Duration::from_millis(50);
    let mut daemon = Daemon::connect(config).await.unwrap();
    let host_id = daemon.enroll().await.unwrap();
    let handle = tokio::spawn(async move {
        let _ = daemon.run().await;
    });
    (host_id, handle)
}

/// Creates a thread bound to an unmanaged environment at `workspace`, appends a
/// user message, and dispatches the resulting run.
///
/// The workspace must exist: the daemon refuses a dispatch whose directory is
/// missing, which is exactly the validation these tests exercise.
fn start_turn(state: &AppState, workspace: &Path, content: &str) -> ThreadId {
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
            loom_domain::EnvironmentKind::Unmanaged,
            Some(workspace.to_string_lossy().into_owned()),
            loom_relay::now_ms(),
        )
        .unwrap();
    let (thread, created) = state
        .registry
        .create_thread(
            Some(state.registry.personal_project_id()),
            Some("turn".into()),
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

/// Polls until `predicate` holds, with a wall-clock bound.
async fn eventually(mut predicate: impl FnMut() -> bool) -> bool {
    for _ in 0..800 {
        if predicate() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    predicate()
}

async fn wait_for_terminal(state: &AppState, thread_id: &ThreadId) -> ThreadStatus {
    wait_for_terminal_for(state, thread_id, Duration::from_secs(10)).await
}

/// The same, with an explicit budget for tests that drive a slower process.
async fn wait_for_terminal_for(
    state: &AppState,
    thread_id: &ThreadId,
    budget: Duration,
) -> ThreadStatus {
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        let status = state.registry.thread(thread_id).unwrap().status;
        if matches!(
            status,
            ThreadStatus::Idle | ThreadStatus::Error | ThreadStatus::Archived
        ) {
            return status;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the thread never left `working` within {budget:?}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Every domain event stored in a thread's scope, in order.
fn thread_events(state: &AppState, thread_id: &ThreadId) -> Vec<Value> {
    let scope = loom_relay::Scope::Thread(thread_id.to_string());
    state
        .relay
        .replay_scope(&scope, 500)
        .unwrap()
        .into_iter()
        .filter_map(|envelope| {
            let frame: Value = serde_json::from_slice(&envelope.payload).ok()?;
            let payload = frame["payload"].as_str()?;
            serde_json::from_str(payload).ok()
        })
        .collect()
}

fn run_events(events: &[Value]) -> Vec<&Value> {
    events
        .iter()
        .filter(|event| event["type"] == "thread_run_event")
        .collect()
}

/// The streamed assistant text, in order.
///
/// The contract carries this as `item/agentMessage/delta` chunks; a client
/// concatenates them into the answer.
fn output_texts(events: &[Value]) -> Vec<String> {
    run_events(events)
        .into_iter()
        .filter(|event| event["event"]["type"] == "item/agentMessage/delta")
        .filter_map(|event| event["event"]["delta"].as_str().map(str::to_owned))
        .collect()
}

/// The terminal contract events, in order.
fn turn_completions(events: &[Value]) -> Vec<&Value> {
    run_events(events)
        .into_iter()
        .filter(|event| event["event"]["type"] == "turn/completed")
        .collect()
}

/// Asserts every run event a turn produced is a valid bb `ThreadEvent`.
///
/// This is the wire contract check: a consumer (bb's projection layer)
/// dispatches on the inner `type` and reads camelCase fields, so a renamed
/// discriminant or field must fail here rather than in the UI.
fn assert_contract_conformant(events: &[Value]) {
    let contract = loom_contract::Contract::load();
    let run_events = run_events(events);
    assert!(
        !run_events.is_empty(),
        "a turn must produce at least one run event"
    );
    for event in &run_events {
        let contract_event = &event["event"];
        let event_type = contract_event["type"]
            .as_str()
            .expect("every run event carries a `type`");
        assert!(
            contract.thread_event_schema(event_type).is_some(),
            "`{event_type}` is not a contract ThreadEvent type"
        );
        let violations = contract.validate_thread_event(contract_event);
        assert!(
            violations.is_empty(),
            "`{event_type}` is not a valid ThreadEvent: {violations:?}\n{contract_event}"
        );
    }
}

/// loom's verdict on the run, from the terminal event's envelope.
///
/// The contract's `turn/completed.status` collapses a deadline, a stale host
/// and a cancellation; loom's `outcome` keeps them apart, so this reads the
/// envelope field and falls back to the contract status for a bare event.
fn terminal_outcome(events: &[Value]) -> Option<String> {
    turn_completions(events).last().and_then(|event| {
        event["outcome"]
            .as_str()
            .or_else(|| event["event"]["status"].as_str())
            .map(str::to_owned)
    })
}

#[tokio::test]
async fn a_provider_turn_runs_end_to_end_and_is_replayable() {
    let dir = tempfile::tempdir().unwrap();
    // The stub speaks ACP over JSON-RPC. The client must still receive the
    // streamed text and tool lifecycle through `session/update`.
    let provider = write_stub(
        dir.path(),
        "provider.sh",
        r#"printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"stub-session","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"hello "}}}}'
printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"stub-session","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"world"}}}}'
printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"stub-session","update":{"sessionUpdate":"tool_call","toolCallId":"c1","title":"ls","kind":"execute","status":"in_progress","rawInput":{"command":"ls"}}}}'
printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"stub-session","update":{"sessionUpdate":"tool_call_update","toolCallId":"c1","status":"completed"}}}'
"#,
    );

    let (url, state) = spawn_server(AppConfig {
        provider_spec: provider.clone(),
        ..AppConfig::default()
    })
    .await;
    let (host_id, daemon) =
        enroll_daemon(&url, None, Some(provider), Duration::from_secs(10)).await;
    assert!(
        eventually(|| state
            .registry
            .host(&host_id)
            .map(|host| host.status == HostStatus::Connected)
            .unwrap_or(false))
        .await
    );

    let thread_id = start_turn(&state, dir.path(), "say hello");
    assert_eq!(
        wait_for_terminal(&state, &thread_id).await,
        ThreadStatus::Idle
    );

    // Replay, exactly as a reconnecting client would read it.
    let events = thread_events(&state, &thread_id);
    assert_eq!(terminal_outcome(&events), Some("completed".into()));
    assert_eq!(output_texts(&events), vec!["hello ", "world"]);

    let tool_calls = run_events(&events)
        .into_iter()
        .filter(|event| event["event"]["type"] == "item/started")
        .filter(|event| event["event"]["item"]["type"] == "commandExecution")
        .count();
    let tool_results = run_events(&events)
        .into_iter()
        .filter(|event| event["event"]["type"] == "item/completed")
        .filter(|event| event["event"]["item"]["type"] == "commandExecution")
        .count();
    assert_eq!(tool_calls, 1);
    assert_eq!(tool_results, 1);

    // The output streamed as run events in the thread scope; that is what a
    // replaying client reconstructs the turn from.
    assert!(events
        .iter()
        .any(|event| event["type"] == "thread_message_added"));
    assert!(events
        .iter()
        .filter(|event| event["type"] == "thread_status_changed")
        .any(|event| event["to"] == "idle"));

    // Every ACP update this turn produced is a valid bb `ThreadEvent`.
    assert_contract_conformant(&events);

    // No run is left in flight.
    assert!(state.runs.is_empty());

    daemon.abort();
    state.shutdown();
}

#[tokio::test]
async fn a_second_turn_reuses_the_persisted_acp_session() {
    let dir = tempfile::tempdir().unwrap();
    let provider = write_stub(
        dir.path(),
        "resume.sh",
        r#"if [ -f "$0.loaded" ]; then
  text=resumed
else
  text=fresh
fi
printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"stub-session","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"'"$text"'"}}}}'
"#,
    );
    let (url, state) = spawn_server(AppConfig {
        provider_spec: provider.clone(),
        ..AppConfig::default()
    })
    .await;
    let (_host_id, daemon) =
        enroll_daemon(&url, None, Some(provider), Duration::from_secs(10)).await;
    assert!(
        eventually(|| state
            .registry
            .hosts()
            .iter()
            .any(|host| host.status == HostStatus::Connected))
        .await
    );

    let thread_id = start_turn(&state, dir.path(), "first turn");
    assert_eq!(
        wait_for_terminal(&state, &thread_id).await,
        ThreadStatus::Idle
    );
    let first_thread = state.registry.thread(&thread_id).unwrap();
    assert_eq!(
        first_thread.provider_session_id.as_deref(),
        Some("stub-session")
    );

    let follow_up = state
        .registry
        .post_message(
            &thread_id,
            MessageRole::User,
            "second turn".to_owned(),
            loom_relay::now_ms(),
        )
        .unwrap();
    for event in &follow_up {
        state.publish_domain_event(event).unwrap();
    }
    let thread = state.registry.thread(&thread_id).unwrap();
    state.dispatch_thread(&thread, "second turn");
    assert_eq!(
        wait_for_terminal(&state, &thread_id).await,
        ThreadStatus::Idle
    );

    let events = thread_events(&state, &thread_id);
    assert_eq!(output_texts(&events), vec!["fresh", "resumed"]);
    daemon.abort();
    state.shutdown();
}

#[tokio::test]
async fn a_provider_runs_in_the_environment_workspace() {
    let dir = tempfile::tempdir().unwrap();
    // The workspace is deliberately a *different* directory from the stub's, so
    // the assertion cannot pass by accident from the daemon's own cwd.
    let workspace = tempfile::tempdir().unwrap();
    let provider = write_stub(
        dir.path(),
        "pwd.sh",
        r#"pwd > pwd.out
"#,
    );

    let (url, state) = spawn_server(AppConfig {
        provider_spec: provider.clone(),
        ..AppConfig::default()
    })
    .await;
    let (host_id, daemon) =
        enroll_daemon(&url, None, Some(provider), Duration::from_secs(10)).await;
    assert!(
        eventually(|| state
            .registry
            .host(&host_id)
            .map(|host| host.status == HostStatus::Connected)
            .unwrap_or(false))
        .await
    );

    let thread_id = start_turn(&state, workspace.path(), "where am i?");
    assert_eq!(
        wait_for_terminal(&state, &thread_id).await,
        ThreadStatus::Idle
    );

    // The provider wrote its cwd where it actually ran. It must be the
    // environment's workspace, not the daemon's process cwd.
    let recorded = std::fs::read_to_string(workspace.path().join("pwd.out")).unwrap();
    assert_eq!(
        std::fs::canonicalize(recorded.trim()).unwrap(),
        std::fs::canonicalize(workspace.path()).unwrap()
    );
    assert_ne!(recorded.trim(), dir.path().to_string_lossy());

    daemon.abort();
    state.shutdown();
}

#[tokio::test]
async fn a_dispatch_to_a_missing_workspace_fails_with_a_clear_reason() {
    let dir = tempfile::tempdir().unwrap();
    let provider = write_stub(
        dir.path(),
        "never-runs.sh",
        r#"# The daemon refuses before the ACP agent is started.
"#,
    );

    let (url, state) = spawn_server(AppConfig {
        provider_spec: provider.clone(),
        ..AppConfig::default()
    })
    .await;
    let (host_id, daemon) =
        enroll_daemon(&url, None, Some(provider), Duration::from_secs(10)).await;
    assert!(
        eventually(|| state
            .registry
            .host(&host_id)
            .map(|host| host.status == HostStatus::Connected)
            .unwrap_or(false))
        .await
    );

    // A path that exists on the server's filesystem but is deliberately removed
    // before the daemon sees it: the daemon must refuse, not fall back to its
    // own cwd.
    let missing = dir.path().join("gone");
    let thread_id = start_turn(&state, &missing, "nowhere to run");
    assert_eq!(
        wait_for_terminal(&state, &thread_id).await,
        ThreadStatus::Error
    );

    let events = thread_events(&state, &thread_id);
    assert_eq!(terminal_outcome(&events), Some("failed".into()));
    let finished = turn_completions(&events)
        .into_iter()
        .next_back()
        .cloned()
        .unwrap();
    assert!(
        finished["event"]["error"]["message"]
            .as_str()
            .is_some_and(|error| error.contains("does not exist")),
        "the failure should name the missing directory: {finished}"
    );
    assert!(state.runs.is_empty());

    daemon.abort();
    state.shutdown();
}

#[tokio::test]
async fn a_crashing_provider_leaves_the_thread_in_error() {
    let dir = tempfile::tempdir().unwrap();
    let provider = write_stub(
        dir.path(),
        "crash.sh",
        r#"printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"stub-session","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"partial"}}}}'
exit 7
"#,
    );

    let (url, state) = spawn_server(AppConfig {
        provider_spec: provider.clone(),
        ..AppConfig::default()
    })
    .await;
    let (host_id, daemon) =
        enroll_daemon(&url, None, Some(provider), Duration::from_secs(10)).await;
    assert!(
        eventually(|| state
            .registry
            .host(&host_id)
            .map(|host| host.status == HostStatus::Connected)
            .unwrap_or(false))
        .await
    );

    let thread_id = start_turn(&state, dir.path(), "crash please");
    assert_eq!(
        wait_for_terminal(&state, &thread_id).await,
        ThreadStatus::Error
    );

    let events = thread_events(&state, &thread_id);
    assert_eq!(terminal_outcome(&events), Some("failed".into()));
    let finished = turn_completions(&events)
        .into_iter()
        .next_back()
        .cloned()
        .unwrap();
    assert!(
        finished["event"]["error"]["message"]
            .as_str()
            .is_some_and(|error| error.contains("exit")),
        "the failure reason should name the exit: {finished}"
    );
    assert!(state.runs.is_empty());

    daemon.abort();
    state.shutdown();
}

#[tokio::test]
async fn a_hanging_provider_is_killed_and_reported_as_timed_out() {
    let dir = tempfile::tempdir().unwrap();
    let provider = write_stub(
        dir.path(),
        "hang.sh",
        r#"sleep 30
"#,
    );

    let (url, state) = spawn_server(AppConfig {
        provider_spec: provider.clone(),
        // A long server deadline, so the daemon's own timeout is what fires.
        run_timeout: Duration::from_secs(60),
        ..AppConfig::default()
    })
    .await;
    let (host_id, daemon) =
        enroll_daemon(&url, None, Some(provider), Duration::from_millis(200)).await;
    assert!(
        eventually(|| state
            .registry
            .host(&host_id)
            .map(|host| host.status == HostStatus::Connected)
            .unwrap_or(false))
        .await
    );

    let thread_id = start_turn(&state, dir.path(), "hang forever");
    assert_eq!(
        wait_for_terminal(&state, &thread_id).await,
        ThreadStatus::Error
    );

    let events = thread_events(&state, &thread_id);
    assert_eq!(terminal_outcome(&events), Some("timed_out".into()));
    assert!(state.runs.is_empty());

    daemon.abort();
    state.shutdown();
}

#[tokio::test]
async fn a_dispatch_missed_while_disconnected_is_replayed_on_reconnect() {
    let dir = tempfile::tempdir().unwrap();
    let provider = write_stub(
        dir.path(),
        "late.sh",
        r#"printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"stub-session","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"caught up"}}}}'
"#,
    );

    let (url, state) = spawn_server(AppConfig {
        provider_spec: provider.clone(),
        ..AppConfig::default()
    })
    .await;

    // First daemon enrolls, so the dispatcher has a connected host, but it is
    // never driven: the dispatch below is written to its scope and not read.
    let mut first_config = DaemonConfig::new(&url, "test-daemon");
    first_config.provider = Some(provider.clone());
    let mut first = Daemon::connect(first_config).await.unwrap();
    let host_id = first.enroll().await.unwrap();

    let thread_id = start_turn(&state, dir.path(), "will you catch up?");
    // The dispatch is now in the host room and retained in the relay.

    first.disconnect().await.unwrap();
    assert!(
        eventually(|| state
            .registry
            .host(&host_id)
            .map(|host| host.status == HostStatus::Disconnected)
            .unwrap_or(false))
        .await,
        "the first daemon should be detached before the reconnect"
    );

    // A restarted daemon presents the same identity and replays its room. The
    // dispatch it missed is delivered late and executed.
    let (reconnected, daemon) = enroll_daemon(
        &url,
        Some(host_id.clone()),
        Some(provider),
        Duration::from_secs(10),
    )
    .await;
    assert_eq!(reconnected, host_id);

    assert_eq!(
        wait_for_terminal(&state, &thread_id).await,
        ThreadStatus::Idle
    );
    let events = thread_events(&state, &thread_id);
    assert_eq!(terminal_outcome(&events), Some("completed".into()));
    assert_eq!(output_texts(&events), vec!["caught up"]);
    assert!(state.runs.is_empty());

    daemon.abort();
    state.shutdown();
}

#[tokio::test]
async fn a_run_on_a_silent_daemon_is_reaped_by_the_stale_heartbeat_sweep() {
    let dir = tempfile::tempdir().unwrap();
    let provider = write_stub(dir.path(), "never.sh", "sleep 30\n");

    let (url, state) = spawn_server(AppConfig {
        provider_spec: provider.clone(),
        run_timeout: Duration::from_secs(60),
        host_stale_after: Duration::from_millis(300),
        reconcile_interval: Duration::from_millis(25),
        ..AppConfig::default()
    })
    .await;

    // Enroll, but never start `run`: the socket stays open while no heartbeat
    // is ever sent. The host is "connected" and then goes silent.
    let mut config = DaemonConfig::new(&url, "silent");
    config.provider = Some(provider);
    let mut daemon = Daemon::connect(config).await.unwrap();
    let host_id = daemon.enroll().await.unwrap();

    let thread_id = start_turn(&state, dir.path(), "nobody is listening");
    assert_eq!(
        wait_for_terminal(&state, &thread_id).await,
        ThreadStatus::Error
    );

    let events = thread_events(&state, &thread_id);
    assert_eq!(terminal_outcome(&events), Some("host_stale".into()));
    assert_eq!(
        state.registry.host(&host_id).unwrap().status,
        HostStatus::Disconnected
    );
    assert!(state.runs.is_empty());

    // Keep the socket (and so the daemon) alive until the assertions are done.
    let _ = daemon.host_id();
    state.shutdown();
}

#[test]
fn the_protocol_version_is_pinned() {
    // A wire-format change has to be deliberate; this fails loudly otherwise.
    assert_eq!(PROTOCOL_VERSION, 3);
}

#[tokio::test]
async fn a_managed_environment_is_provisioned_by_the_daemon() {
    let root = tempfile::tempdir().unwrap();
    let (url, state) = spawn_server(AppConfig::default()).await;

    let mut config = DaemonConfig::new(&url, "test-daemon");
    config.environment_root = root.path().to_path_buf();
    config.heartbeat_interval = Duration::from_millis(50);
    let mut daemon = Daemon::connect(config).await.unwrap();
    let host_id = daemon.enroll().await.unwrap();
    let handle = tokio::spawn(async move {
        let _ = daemon.run().await;
    });

    let (environment, _) = state
        .registry
        .create_environment(
            Some(state.registry.personal_project_id()),
            host_id,
            EnvironmentKind::Managed,
            None,
            loom_relay::now_ms(),
        )
        .unwrap();
    assert!(matches!(
        state.provision_environment(&environment.id),
        loom_server::environments::ProvisionOutcome::Dispatched(_)
    ));

    assert!(
        eventually(|| state
            .registry
            .environment(&environment.id)
            .map(|environment| environment.status == EnvironmentStatus::Ready)
            .unwrap_or(false))
        .await,
        "the daemon should provision the workspace"
    );

    let stored = state.registry.environment(&environment.id).unwrap();
    let path = stored
        .path
        .as_deref()
        .expect("a ready environment has a path");
    assert_eq!(
        path,
        root.path()
            .join(environment.id.to_string())
            .to_string_lossy()
    );
    assert!(Path::new(path).is_dir());

    handle.abort();
    state.shutdown();
}

#[tokio::test]
async fn a_failing_provision_records_the_daemon_reason() {
    let root = tempfile::tempdir().unwrap();
    // A regular file where the workspace root should be: every create fails.
    let not_a_dir = root.path().join("not-a-dir");
    std::fs::write(&not_a_dir, "x").unwrap();

    let (url, state) = spawn_server(AppConfig::default()).await;
    let mut config = DaemonConfig::new(&url, "test-daemon");
    config.environment_root = not_a_dir;
    config.heartbeat_interval = Duration::from_millis(50);
    let mut daemon = Daemon::connect(config).await.unwrap();
    let host_id = daemon.enroll().await.unwrap();
    let handle = tokio::spawn(async move {
        let _ = daemon.run().await;
    });

    let (environment, _) = state
        .registry
        .create_environment(
            Some(state.registry.personal_project_id()),
            host_id,
            EnvironmentKind::Managed,
            None,
            loom_relay::now_ms(),
        )
        .unwrap();
    state.provision_environment(&environment.id);

    assert!(
        eventually(|| state
            .registry
            .environment(&environment.id)
            .map(|environment| environment.status == EnvironmentStatus::Error)
            .unwrap_or(false))
        .await,
        "a failed provision should reach `error`"
    );
    let stored = state.registry.environment(&environment.id).unwrap();
    assert!(
        stored
            .error
            .as_deref()
            .is_some_and(|error| error.contains("not-a-dir")),
        "the daemon's reason should survive: {:?}",
        stored.error
    );

    handle.abort();
    state.shutdown();
}

/// The real provider, not a stub.
///
/// Ignored by default because it runs the actual `pi` CLI. It asserts the
/// terminal guarantee and that real Pi frames reached the bridge; it does not
/// require a model to answer, so it passes on a machine with no credentials
/// (the run simply ends `timed_out`). Run it explicitly with
/// `cargo test -p loom-daemon --test provider_e2e -- --ignored`.
#[tokio::test]
#[ignore = "runs the real `pi` CLI"]
async fn the_real_pi_process_streams_through_the_bridge() {
    let dir = tempfile::tempdir().unwrap();
    let (url, state) = spawn_server(AppConfig::default()).await;
    // 20s is long enough to see Pi's startup frames and short enough that a
    // model-less environment still ends the turn.
    let (host_id, daemon) = enroll_daemon(&url, None, None, Duration::from_secs(20)).await;
    assert!(
        eventually(|| state
            .registry
            .host(&host_id)
            .map(|host| host.status == HostStatus::Connected)
            .unwrap_or(false))
        .await
    );

    let thread_id = start_turn(&state, dir.path(), "Reply with exactly the word: pong");
    let status = wait_for_terminal_for(&state, &thread_id, Duration::from_secs(60)).await;
    assert!(
        matches!(status, ThreadStatus::Idle | ThreadStatus::Error),
        "a run must end in idle or error, got {status}"
    );

    let events = thread_events(&state, &thread_id);
    let started = run_events(&events)
        .into_iter()
        .any(|event| event["event"]["type"] == "turn/started");
    assert!(started, "the real Pi process should report `turn/started`");
    assert!(
        terminal_outcome(&events).is_some(),
        "every run must end with exactly one terminal event"
    );

    // Every event a real Pi turn produced must be a valid bb `ThreadEvent`.
    // This is the conformance check the contract export exists for: the frame
    // a consumer receives is checked against `contracts/bb/thread-event.json`,
    // not against a hand-written expectation.
    assert_contract_conformant(&events);

    daemon.abort();
    state.shutdown();
}

/// A reconnect that missed more dispatches than one replay page must still
/// receive every one of them.
///
/// The server pages a cursor replay from the **oldest** frame after the cursor
/// precisely so this works. The daemon advances its persisted resume cursor as
/// it applies frames, so a single-page replay would move that cursor to the
/// newest frame and silently strand every dispatch in between — they would
/// never be retried. Here `replay_limit` is 4 and 10 runs are queued after the
/// cursor.
#[tokio::test]
async fn a_reconnect_recovers_more_dispatches_than_one_replay_page() {
    let dir = tempfile::tempdir().unwrap();
    let provider = write_stub(
        dir.path(),
        "quick.sh",
        r#"# A quick ACP prompt completes with no content.
"#,
    );

    let (url, state) = spawn_server(AppConfig {
        provider_spec: provider.clone(),
        run_timeout: Duration::from_secs(60),
        ..AppConfig::default()
    })
    .await;

    // A daemon enrolls so the dispatcher has a host, but it never runs its
    // event loop: every dispatch lands in its room and is left unread.
    let mut first_config = DaemonConfig::new(&url, "test-daemon");
    first_config.provider = Some(provider.clone());
    let mut first = Daemon::connect(first_config).await.unwrap();
    let host_id = first.enroll().await.unwrap();
    let host_scope = loom_relay::Scope::Host(host_id.to_string());

    // A warm-up dispatch establishes the point a restarting daemon would have
    // cursored past before it went away.
    start_turn(&state, dir.path(), "warm-up");
    let cursor = state
        .relay
        .replay_scope(&host_scope, 100)
        .unwrap()
        .last()
        .expect("the warm-up dispatch is in the host room")
        .event_id;

    // The backlog: strictly after the cursor, and more than one page.
    const BACKLOG: usize = 10;
    let mut threads = Vec::new();
    for i in 0..BACKLOG {
        threads.push(start_turn(&state, dir.path(), &format!("backlog {i}")));
    }

    first.disconnect().await.unwrap();
    assert!(
        eventually(|| state
            .registry
            .host(&host_id)
            .map(|host| host.status == HostStatus::Disconnected)
            .unwrap_or(false))
        .await
    );

    // Restart with the persisted cursor and a page limit far below the backlog.
    let mut resume_config = DaemonConfig::new(&url, "test-daemon");
    resume_config.host_id = Some(host_id.clone());
    resume_config.provider = Some(provider);
    resume_config.run_timeout = Duration::from_secs(60);
    resume_config.heartbeat_interval = Duration::from_millis(50);
    resume_config.resume_cursor = Some(cursor);
    resume_config.replay_limit = 4;
    let mut resumed = Daemon::connect(resume_config).await.unwrap();
    assert_eq!(resumed.enroll().await.unwrap(), host_id);
    let daemon = tokio::spawn(async move {
        let _ = resumed.run().await;
    });

    // Every queued run must reach a terminal state, not just the newest page.
    for (i, thread_id) in threads.iter().enumerate() {
        assert_eq!(
            wait_for_terminal(&state, thread_id).await,
            ThreadStatus::Idle,
            "dispatch {i} must run; a single-page replay would strand it"
        );
        let events = thread_events(&state, thread_id);
        assert_eq!(terminal_outcome(&events), Some("completed".into()));
    }

    daemon.abort();
    state.shutdown();
}

// --- permissions: the agent asks a user, and the answer gets back ------------

/// A minimal HTTP client for the two routes the permission tests use.
///
/// The B3 conformance suite has the same helper; it is duplicated here because
/// these tests are about the *daemon* path and must not depend on the server's
/// test crate.
async fn http(addr: &str, method: &str, path: &str, body: Option<&Value>) -> (u16, Value) {
    let payload = body.map(Value::to_string);
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let head = match &payload {
        Some(payload) => format!(
            "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{payload}",
            payload.len()
        ),
        None => format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n"),
    };
    stream.write_all(head.as_bytes()).await.unwrap();
    stream.flush().await.unwrap();
    let mut raw = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), stream.read_to_end(&mut raw))
        .await
        .expect("HTTP response timed out")
        .unwrap();
    let text = String::from_utf8(raw).unwrap();
    let (head, body) = text
        .split_once("\r\n\r\n")
        .expect("response had no body separator");
    let status = head
        .split(' ')
        .nth(1)
        .and_then(|code| code.parse().ok())
        .expect("no HTTP status line");
    let body = serde_json::from_str(body).unwrap_or(Value::Null);
    (status, body)
}

/// An ACP agent that asks permission once and reports the decision it received.
///
/// The decision is written to `$0.decision`, so the test can assert what the
/// *agent* was told rather than what loom recorded. The stub blocks on the
/// permission response before finishing the turn, which is what makes the
/// blocked-turn property observable.
const PERMISSION_STUB: &str = r#"
printf '%s\n' '{"jsonrpc":"2.0","id":"perm-1","method":"session/request_permission","params":{"sessionId":"stub-session","toolCall":{"toolCallId":"call-9","title":"Run rm -rf /","kind":"execute","status":"pending"},"options":[{"optionId":"allow-once","name":"Allow once","kind":"allow_once"},{"optionId":"deny","name":"Deny","kind":"reject_once"}]}}'
while IFS= read -r line; do
  case "$line" in
    *'"id":"perm-1"'*)
      printf '%s' "$line" > "$0.decision"
      break
      ;;
  esac
done
printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"stub-session","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"decision received"}}}}'
"#;

/// The permission request reaches the interaction routes, and the recorded
/// resolution reaches the blocked agent.
///
/// This is the whole bridge, end to end: an ACP agent asks, the daemon forwards
/// the question up its socket, the control plane records a durable interaction,
/// a client answers over HTTP, and the answer travels back through the relay to
/// the agent's own request.
#[tokio::test]
async fn a_permission_request_is_answered_through_the_interaction_routes() {
    let dir = tempfile::tempdir().unwrap();
    let provider = write_stub(dir.path(), "permission.sh", PERMISSION_STUB);
    let (url, state) = spawn_server(AppConfig {
        provider_spec: provider.clone(),
        run_timeout: Duration::from_secs(30),
        ..AppConfig::default()
    })
    .await;
    let (host_id, daemon) =
        enroll_daemon(&url, None, Some(provider), Duration::from_secs(30)).await;
    assert!(
        eventually(|| state
            .registry
            .host(&host_id)
            .map(|host| host.status == HostStatus::Connected)
            .unwrap_or(false))
        .await
    );

    let thread_id = start_turn(&state, dir.path(), "do the thing");

    // The question must appear on the thread, in `pending`, while the turn is
    // still blocked. Nothing may answer it on the daemon's behalf.
    let addr = url.trim_start_matches("http://").to_string();
    let listed = eventually_async(|| {
        let addr = addr.clone();
        let thread_id = thread_id.clone();
        async move {
            let (status, body) = http(
                &addr,
                "GET",
                &format!("/api/v1/threads/{thread_id}/interactions"),
                None,
            )
            .await;
            (status == 200 && body.as_array().is_some_and(|rows| !rows.is_empty())).then_some(body)
        }
    })
    .await
    .expect("the permission request must appear as an interaction");

    assert_eq!(listed[0]["payload"]["kind"], "approval");
    assert_eq!(listed[0]["status"], "pending");
    assert_eq!(
        listed[0]["providerThreadId"], "stub-session",
        "the agent's own session id is what correlates the question with its run"
    );
    assert_eq!(
        listed[0]["payload"]["subject"]["kind"], "tool_use",
        "the subject is the tool call the agent asked about"
    );
    assert_eq!(
        state.registry.thread(&thread_id).unwrap().status,
        ThreadStatus::Working,
        "the turn is blocked on the answer, not finished"
    );

    // Answer it as a user would.
    let interaction_id = listed[0]["id"].as_str().unwrap().to_owned();
    let (status, resolved) = http(
        &addr,
        "POST",
        &format!("/api/v1/threads/{thread_id}/interactions/{interaction_id}/resolve"),
        Some(&serde_json::json!({
            "decision": "allow_once",
            "grantedPermissions": {
                "network": { "enabled": true },
                "fileSystem": { "read": [], "write": [] },
            },
        })),
    )
    .await;
    assert_eq!(status, 200, "{resolved}");
    assert_eq!(resolved["status"], "resolved");

    // The agent receives the decision and finishes the turn.
    assert_eq!(
        wait_for_terminal(&state, &thread_id).await,
        ThreadStatus::Idle
    );
    let events = thread_events(&state, &thread_id);
    assert_eq!(terminal_outcome(&events), Some("completed".into()));
    assert_eq!(output_texts(&events), vec!["decision received"]);

    let decision = std::fs::read_to_string(dir.path().join("permission.sh.decision"))
        .expect("the agent must have received a permission response");
    assert!(
        decision.contains("\"optionId\":\"allow-once\""),
        "an `allow_once` decision must select the agent's once-option: {decision}"
    );

    // The interaction history is on the thread scope, so a client that was not
    // attached when the question was asked can still reconstruct it.
    assert!(events.iter().any(|event| {
        event["type"] == "thread_interaction_changed" && event["interaction"]["status"] == "pending"
    }));
    assert!(events.iter().any(|event| {
        event["type"] == "thread_interaction_changed"
            && event["interaction"]["status"] == "resolved"
    }));
    assert_contract_conformant(&events);

    daemon.abort();
    state.shutdown();
}

/// A denial reaches the agent as a refusal of the option it offered, never as
/// an approval.
#[tokio::test]
async fn a_denied_permission_selects_the_agents_rejecting_option() {
    let dir = tempfile::tempdir().unwrap();
    let provider = write_stub(dir.path(), "deny.sh", PERMISSION_STUB);
    let (url, state) = spawn_server(AppConfig {
        provider_spec: provider.clone(),
        run_timeout: Duration::from_secs(30),
        ..AppConfig::default()
    })
    .await;
    let (host_id, daemon) =
        enroll_daemon(&url, None, Some(provider), Duration::from_secs(30)).await;
    assert!(
        eventually(|| state
            .registry
            .host(&host_id)
            .map(|host| host.status == HostStatus::Connected)
            .unwrap_or(false))
        .await
    );
    let thread_id = start_turn(&state, dir.path(), "do the thing");
    let addr = url.trim_start_matches("http://").to_string();

    let listed = eventually_async(|| {
        let addr = addr.clone();
        let thread_id = thread_id.clone();
        async move {
            let (status, body) = http(
                &addr,
                "GET",
                &format!("/api/v1/threads/{thread_id}/interactions"),
                None,
            )
            .await;
            (status == 200 && body.as_array().is_some_and(|rows| !rows.is_empty())).then_some(body)
        }
    })
    .await
    .expect("the request must appear");
    let interaction_id = listed[0]["id"].as_str().unwrap().to_owned();

    let (status, resolved) = http(
        &addr,
        "POST",
        &format!("/api/v1/threads/{thread_id}/interactions/{interaction_id}/resolve"),
        Some(&serde_json::json!({ "decision": "deny" })),
    )
    .await;
    assert_eq!(status, 200, "{resolved}");

    assert_eq!(
        wait_for_terminal(&state, &thread_id).await,
        ThreadStatus::Idle
    );
    let decision = std::fs::read_to_string(dir.path().join("deny.sh.decision"))
        .expect("the agent must have received a permission response");
    assert!(
        decision.contains("\"optionId\":\"deny\""),
        "a denial must select the rejecting option, never an allow: {decision}"
    );
    assert!(
        !decision.contains("allow-once"),
        "a denial must not contain an allowing option: {decision}"
    );

    daemon.abort();
    state.shutdown();
}

/// A permission request nobody answers is cancelled when the run ends, and the
/// interaction does not stay pending.
///
/// The regression this guards is the old auto-allow policy *and* its mirror: a
/// question left `pending` forever keeps the thread's pending-interaction flag
/// set. Cancellation is the only outcome when no user answers.
#[tokio::test]
async fn an_unanswered_permission_is_cancelled_when_the_run_ends() {
    let dir = tempfile::tempdir().unwrap();
    let provider = write_stub(dir.path(), "unanswered.sh", PERMISSION_STUB);
    let (url, state) = spawn_server(AppConfig {
        provider_spec: provider.clone(),
        // Short enough that the run deadline reaps the blocked turn.
        run_timeout: Duration::from_millis(150),
        ..AppConfig::default()
    })
    .await;
    let (host_id, daemon) =
        enroll_daemon(&url, None, Some(provider), Duration::from_millis(150)).await;
    assert!(
        eventually(|| state
            .registry
            .host(&host_id)
            .map(|host| host.status == HostStatus::Connected)
            .unwrap_or(false))
        .await
    );
    let thread_id = start_turn(&state, dir.path(), "do the thing");
    let addr = url.trim_start_matches("http://").to_string();

    let listed = eventually_async(|| {
        let addr = addr.clone();
        let thread_id = thread_id.clone();
        async move {
            let (status, body) = http(
                &addr,
                "GET",
                &format!("/api/v1/threads/{thread_id}/interactions"),
                None,
            )
            .await;
            (status == 200 && body.as_array().is_some_and(|rows| !rows.is_empty())).then_some(body)
        }
    })
    .await
    .expect("the question was asked");

    // The run deadline reaps the blocked turn; the interaction must settle with
    // it rather than stay open.
    let _ = wait_for_terminal_for(&state, &thread_id, Duration::from_secs(10)).await;
    let interaction_id = listed[0]["id"].as_str().unwrap();
    let (status, fetched) = http(
        &addr,
        "GET",
        &format!("/api/v1/threads/{thread_id}/interactions/{interaction_id}"),
        None,
    )
    .await;
    assert_eq!(status, 200, "{fetched}");
    assert_eq!(
        fetched["status"], "interrupted",
        "an unanswered question must settle, not stay pending: {fetched}"
    );
    assert_eq!(fetched["resolution"], Value::Null);

    // The agent was *not* told "allowed". Two honest endings are possible here
    // and both are correct: the broker settles the held request as cancelled
    // (the file is written with no allow in it), or the run deadline tears the
    // ACP connection down before the answer can be written (no file). What must
    // never happen is an allow.
    if let Ok(decision) = std::fs::read_to_string(dir.path().join("unanswered.sh.decision")) {
        assert!(
            !decision.contains("allow-once"),
            "an unanswered request must never be answered with an allow: {decision}"
        );
    }

    daemon.abort();
    state.shutdown();
}

/// Polls an async predicate, with the same wall-clock bound as `eventually`.
async fn eventually_async<T, F, Fut>(mut predicate: F) -> Option<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Option<T>>,
{
    for _ in 0..400 {
        if let Some(value) = predicate().await {
            return Some(value);
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    predicate().await
}
