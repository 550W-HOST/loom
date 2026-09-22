//! Provider execution end to end, over real sockets and real processes.
//!
//! Each test starts a real server, enrolls a real worker, and runs a real
//! provider process — a small stub that speaks ACP over JSON-RPC. The ACP
//! client and translator are exercised directly in the crate's unit tests; here
//! the point is the path around them: dispatch through the relay, provider
//! output reported back, and the thread leaving `working` with exactly one
//! terminal event, including when the provider crashes, hangs or the worker
//! disappears.

use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use loom_domain::{
    EnvironmentKind, EnvironmentStatus, HostId, HostStatus, MessageRole, ThreadId, ThreadStatus,
};
use loom_provider_protocol::ProviderSpec;
use loom_server::http::router;
use loom_server::state::{AppConfig, AppState};
use loom_server::PROTOCOL_VERSION;
use loom_worker::{Worker, WorkerConfig};
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

/// Connects and enrolls a worker, then drives it on a background task.
async fn enroll_worker(
    url: &str,
    data_dir: Option<PathBuf>,
    host_id: Option<HostId>,
    provider: Option<ProviderSpec>,
    run_timeout: Duration,
) -> (HostId, tokio::task::JoinHandle<()>) {
    let mut config = WorkerConfig::new(url, "test-worker").without_discovery();
    if let Some(data_dir) = data_dir {
        config.data_dir = data_dir;
    }
    config.host_id = host_id;
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

/// Creates a thread bound to an unmanaged environment at `workspace`, appends a
/// user message, and dispatches the resulting run.
///
/// The workspace must exist: the worker refuses a dispatch whose directory is
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
        providers: vec![provider.clone()],
        ..AppConfig::default()
    })
    .await;
    let (host_id, worker) =
        enroll_worker(&url, None, None, Some(provider), Duration::from_secs(10)).await;
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

    worker.abort();
    state.shutdown().unwrap();
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
        providers: vec![provider.clone()],
        ..AppConfig::default()
    })
    .await;
    let (_host_id, worker) =
        enroll_worker(&url, None, None, Some(provider), Duration::from_secs(10)).await;
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
    worker.abort();
    state.shutdown().unwrap();
}

#[tokio::test]
async fn a_provider_runs_in_the_environment_workspace() {
    let dir = tempfile::tempdir().unwrap();
    // The workspace is deliberately a *different* directory from the stub's, so
    // the assertion cannot pass by accident from the worker's own cwd.
    let workspace = tempfile::tempdir().unwrap();
    let provider = write_stub(
        dir.path(),
        "pwd.sh",
        r#"pwd > pwd.out
"#,
    );

    let (url, state) = spawn_server(AppConfig {
        providers: vec![provider.clone()],
        ..AppConfig::default()
    })
    .await;
    let (host_id, worker) =
        enroll_worker(&url, None, None, Some(provider), Duration::from_secs(10)).await;
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
    // environment's workspace, not the worker's process cwd.
    let recorded = std::fs::read_to_string(workspace.path().join("pwd.out")).unwrap();
    assert_eq!(
        std::fs::canonicalize(recorded.trim()).unwrap(),
        std::fs::canonicalize(workspace.path()).unwrap()
    );
    assert_ne!(recorded.trim(), dir.path().to_string_lossy());

    worker.abort();
    state.shutdown().unwrap();
}

#[tokio::test]
async fn a_dispatch_to_a_missing_workspace_fails_with_a_clear_reason() {
    let dir = tempfile::tempdir().unwrap();
    let provider = write_stub(
        dir.path(),
        "never-runs.sh",
        r#"# The worker refuses before the ACP agent is started.
"#,
    );

    let (url, state) = spawn_server(AppConfig {
        providers: vec![provider.clone()],
        ..AppConfig::default()
    })
    .await;
    let (host_id, worker) =
        enroll_worker(&url, None, None, Some(provider), Duration::from_secs(10)).await;
    assert!(
        eventually(|| state
            .registry
            .host(&host_id)
            .map(|host| host.status == HostStatus::Connected)
            .unwrap_or(false))
        .await
    );

    // A path that exists on the server's filesystem but is deliberately removed
    // before the worker sees it: the worker must refuse, not fall back to its
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

    worker.abort();
    state.shutdown().unwrap();
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
        providers: vec![provider.clone()],
        ..AppConfig::default()
    })
    .await;
    let (host_id, worker) =
        enroll_worker(&url, None, None, Some(provider), Duration::from_secs(10)).await;
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

    worker.abort();
    state.shutdown().unwrap();
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
        providers: vec![provider.clone()],
        // A long server deadline, so the worker's own timeout is what fires.
        run_timeout: Duration::from_secs(60),
        ..AppConfig::default()
    })
    .await;
    let (host_id, worker) =
        enroll_worker(&url, None, None, Some(provider), Duration::from_millis(200)).await;
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

    worker.abort();
    state.shutdown().unwrap();
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
        providers: vec![provider.clone()],
        ..AppConfig::default()
    })
    .await;

    // First worker enrolls, so the dispatcher has a connected host, but it is
    // never driven: the dispatch below is written to its scope and not read.
    let mut first_config = WorkerConfig::new(&url, "test-worker").without_discovery();
    first_config.provider = Some(provider.clone());
    let mut first = Worker::connect(first_config).await.unwrap();
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
        "the first worker should be detached before the reconnect"
    );

    // A restarted worker presents the same identity and replays its room. The
    // dispatch it missed is delivered late and executed.
    let (reconnected, worker) = enroll_worker(
        &url,
        None,
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

    worker.abort();
    state.shutdown().unwrap();
}

#[tokio::test]
async fn a_run_on_a_silent_worker_is_reaped_by_the_stale_heartbeat_sweep() {
    let dir = tempfile::tempdir().unwrap();
    let provider = write_stub(dir.path(), "never.sh", "sleep 30\n");

    let (url, state) = spawn_server(AppConfig {
        providers: vec![provider.clone()],
        run_timeout: Duration::from_secs(60),
        host_stale_after: Duration::from_millis(300),
        reconcile_interval: Duration::from_millis(25),
        ..AppConfig::default()
    })
    .await;

    // Enroll, but never start `run`: the socket stays open while no heartbeat
    // is ever sent. The host is "connected" and then goes silent.
    let mut config = WorkerConfig::new(&url, "silent").without_discovery();
    config.provider = Some(provider);
    let mut worker = Worker::connect(config).await.unwrap();
    let host_id = worker.enroll().await.unwrap();

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

    // Keep the socket (and so the worker) alive until the assertions are done.
    let _ = worker.host_id();
    state.shutdown().unwrap();
}

#[test]
fn the_protocol_version_is_pinned() {
    // A wire-format change has to be deliberate; this fails loudly otherwise.
    assert_eq!(PROTOCOL_VERSION, 4);
}

#[tokio::test]
async fn a_managed_environment_is_provisioned_by_the_worker() {
    let root = tempfile::tempdir().unwrap();
    let (url, state) = spawn_server(AppConfig::default()).await;

    let mut config = WorkerConfig::new(&url, "test-worker").without_discovery();
    config.environment_root = root.path().to_path_buf();
    config.heartbeat_interval = Duration::from_millis(50);
    let mut worker = Worker::connect(config).await.unwrap();
    let host_id = worker.enroll().await.unwrap();
    let handle = tokio::spawn(async move {
        let _ = worker.run().await;
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
        "the worker should provision the workspace"
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
    state.shutdown().unwrap();
}

#[tokio::test]
async fn a_failing_provision_records_the_worker_reason() {
    let root = tempfile::tempdir().unwrap();
    // A regular file where the workspace root should be: every create fails.
    let not_a_dir = root.path().join("not-a-dir");
    std::fs::write(&not_a_dir, "x").unwrap();

    let (url, state) = spawn_server(AppConfig::default()).await;
    let mut config = WorkerConfig::new(&url, "test-worker").without_discovery();
    config.environment_root = not_a_dir;
    config.heartbeat_interval = Duration::from_millis(50);
    let mut worker = Worker::connect(config).await.unwrap();
    let host_id = worker.enroll().await.unwrap();
    let handle = tokio::spawn(async move {
        let _ = worker.run().await;
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
        "the worker's reason should survive: {:?}",
        stored.error
    );

    handle.abort();
    state.shutdown().unwrap();
}

/// The real provider, not a stub.
///
/// Ignored by default because it runs the actual `pi` CLI. It asserts the
/// terminal guarantee and that real Pi frames reached the bridge; it does not
/// require a model to answer, so it passes on a machine with no credentials
/// (the run simply ends `timed_out`). Run it explicitly with
/// `cargo test -p loom-worker --test provider_e2e -- --ignored`.
///
/// **A model that answered must not end in `timed_out`.** The run timeout is a
/// backstop for a provider that never reports completion; a turn that produced
/// an assistant message and then sat there is the W-623 hang — real Pi reported
/// `agent_settled`, pi-acp settled the turn and resolved `session/prompt`, and
/// the v2 completion notification was dropped (its outbound connector had died
/// on an unconvertible update), so loom waited for a timeout that now closes the
/// turn from the prompt response instead.
#[tokio::test]
#[ignore = "runs the real `pi` CLI"]
async fn the_real_pi_process_streams_through_the_bridge() {
    let dir = tempfile::tempdir().unwrap();
    let (url, state) = spawn_server(AppConfig::default()).await;
    // A minute is long enough for Pi's startup and a model round trip on a
    // machine with credentials, and short enough that a model-less environment
    // still ends the turn by itself.
    let (host_id, worker) = enroll_worker(&url, None, None, None, Duration::from_secs(60)).await;
    assert!(
        eventually(|| state
            .registry
            .host(&host_id)
            .map(|host| host.status == HostStatus::Connected)
            .unwrap_or(false))
        .await
    );

    let thread_id = start_turn(&state, dir.path(), "Reply with exactly the word: pong");
    let status = wait_for_terminal_for(&state, &thread_id, Duration::from_secs(120)).await;
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
    // Only a machine with credentials sees text here; such a machine must see
    // the turn close on its own (W-623).
    if !output_texts(&events).is_empty() {
        assert_ne!(
            terminal_outcome(&events),
            Some("timed_out".to_string()),
            "a turn that produced an answer must not end in a timeout"
        );
    }

    // Every event a real Pi turn produced must be a valid bb `ThreadEvent`.
    // This is the conformance check the contract export exists for: the frame
    // a consumer receives is checked against `contracts/bb/thread-event.json`,
    // not against a hand-written expectation.
    assert_contract_conformant(&events);

    worker.abort();
    state.shutdown().unwrap();
}

/// A reconnect that missed more dispatches than one replay page must still
/// receive every one of them.
///
/// The server pages a cursor replay from the **oldest** frame after the cursor
/// precisely so this works. The worker advances its persisted resume cursor as
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
        providers: vec![provider.clone()],
        run_timeout: Duration::from_secs(60),
        ..AppConfig::default()
    })
    .await;

    // A worker enrolls so the dispatcher has a host, but it never runs its
    // event loop: every dispatch lands in its room and is left unread.
    let mut first_config = WorkerConfig::new(&url, "test-worker").without_discovery();
    first_config.provider = Some(provider.clone());
    let mut first = Worker::connect(first_config).await.unwrap();
    let host_id = first.enroll().await.unwrap();
    let host_scope = loom_relay::Scope::Host(host_id.to_string());

    // A warm-up dispatch establishes the point a restarting worker would have
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
    let mut resume_config = WorkerConfig::new(&url, "test-worker").without_discovery();
    resume_config.host_id = Some(host_id.clone());
    resume_config.provider = Some(provider);
    resume_config.run_timeout = Duration::from_secs(60);
    resume_config.heartbeat_interval = Duration::from_millis(50);
    resume_config.resume_cursor = Some(cursor);
    resume_config.replay_limit = 4;
    let mut resumed = Worker::connect(resume_config).await.unwrap();
    assert_eq!(resumed.enroll().await.unwrap(), host_id);
    let worker = tokio::spawn(async move {
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

    worker.abort();
    state.shutdown().unwrap();
}

// --- permissions: the agent asks a user, and the answer gets back ------------

/// A minimal HTTP client for the two routes the permission tests use.
///
/// The B3 conformance suite has the same helper; it is duplicated here because
/// these tests are about the *worker* path and must not depend on the server's
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
/// This is the whole bridge, end to end: an ACP agent asks, the worker forwards
/// the question up its socket, the control plane records a durable interaction,
/// a client answers over HTTP, and the answer travels back through the relay to
/// the agent's own request.
#[tokio::test]
async fn a_permission_request_is_answered_through_the_interaction_routes() {
    let dir = tempfile::tempdir().unwrap();
    let provider = write_stub(dir.path(), "permission.sh", PERMISSION_STUB);
    let (url, state) = spawn_server(AppConfig {
        providers: vec![provider.clone()],
        run_timeout: Duration::from_secs(30),
        ..AppConfig::default()
    })
    .await;
    let (host_id, worker) =
        enroll_worker(&url, None, None, Some(provider), Duration::from_secs(30)).await;
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
    // still blocked. Nothing may answer it on the worker's behalf.
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
            "grantedPermissions": null,
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

    worker.abort();
    state.shutdown().unwrap();
}

/// A denial reaches the agent as a refusal of the option it offered, never as
/// an approval.
#[tokio::test]
async fn a_denied_permission_selects_the_agents_rejecting_option() {
    let dir = tempfile::tempdir().unwrap();
    let provider = write_stub(dir.path(), "deny.sh", PERMISSION_STUB);
    let (url, state) = spawn_server(AppConfig {
        providers: vec![provider.clone()],
        run_timeout: Duration::from_secs(30),
        ..AppConfig::default()
    })
    .await;
    let (host_id, worker) =
        enroll_worker(&url, None, None, Some(provider), Duration::from_secs(30)).await;
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

    worker.abort();
    state.shutdown().unwrap();
}

/// A client cancellation settles the durable row and reaches the blocked ACP
/// request as `Cancelled`, rather than merely hiding the prompt in the UI.
#[tokio::test]
async fn a_client_cancelled_permission_unblocks_the_agent() {
    let dir = tempfile::tempdir().unwrap();
    let provider = write_stub(dir.path(), "cancel.sh", PERMISSION_STUB);
    let (url, state) = spawn_server(AppConfig {
        providers: vec![provider.clone()],
        run_timeout: Duration::from_secs(30),
        ..AppConfig::default()
    })
    .await;
    let (host_id, worker) =
        enroll_worker(&url, None, None, Some(provider), Duration::from_secs(30)).await;
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
    assert_eq!(listed[0]["payload"]["subject"]["kind"], "tool_use");
    let interaction_id = listed[0]["id"].as_str().unwrap().to_owned();

    let (status, cancelled) = http(
        &addr,
        "POST",
        &format!("/api/v1/threads/{thread_id}/interactions/{interaction_id}/cancel"),
        None,
    )
    .await;
    assert_eq!(status, 200, "{cancelled}");
    assert_eq!(cancelled["status"], "interrupted");
    assert_eq!(cancelled["resolution"], Value::Null);

    assert_eq!(
        wait_for_terminal(&state, &thread_id).await,
        ThreadStatus::Idle,
        "client cancellation must unblock the provider immediately"
    );
    let events = thread_events(&state, &thread_id);
    assert_eq!(terminal_outcome(&events), Some("completed".into()));
    assert_eq!(output_texts(&events), vec!["decision received"]);

    let decision = std::fs::read_to_string(dir.path().join("cancel.sh.decision"))
        .expect("the agent must have received a cancellation response");
    assert!(
        decision.contains("\"outcome\":{\"outcome\":\"cancelled\"}"),
        "client cancellation must become ACP Cancelled: {decision}"
    );
    assert!(
        !decision.contains("optionId"),
        "cancellation must not select an allow or deny option: {decision}"
    );

    worker.abort();
    state.shutdown().unwrap();
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
        providers: vec![provider.clone()],
        // Short enough that the run deadline reaps the blocked turn.
        run_timeout: Duration::from_millis(150),
        ..AppConfig::default()
    })
    .await;
    let (host_id, worker) =
        enroll_worker(&url, None, None, Some(provider), Duration::from_millis(150)).await;
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

    worker.abort();
    state.shutdown().unwrap();
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

/* ------------------------------------------------------------------ */
/* Automations                                                         */
/* ------------------------------------------------------------------ */

/// The automation fixture: an agent automation in the personal project whose
/// execution reuses an unmanaged environment at `workspace`.
fn automation_fixture(
    state: &AppState,
    host_id: &HostId,
    workspace: &Path,
    trigger: loom_domain::automation::AutomationTrigger,
) -> loom_domain::automation::Automation {
    let project = state.registry.personal_project_id();
    let (environment, events) = state
        .registry
        .create_environment(
            Some(project.clone()),
            host_id.clone(),
            EnvironmentKind::Unmanaged,
            Some(workspace.to_string_lossy().into_owned()),
            loom_relay::now_ms(),
        )
        .unwrap();
    for event in &events {
        state.publish_domain_event(event).unwrap();
    }
    state
        .automations
        .create(
            project,
            loom_domain::automation::NewAutomation {
                name: "nightly summary".into(),
                enabled: true,
                trigger,
                execution: loom_domain::automation::AutomationExecution::Agent(
                    loom_domain::automation::AgentExecution {
                        prompt: "summarise the repository".into(),
                        provider_id: "pi".into(),
                        model: "pi/default".into(),
                        reasoning_level: loom_domain::ReasoningLevel::from("medium"),
                        service_tier: None,
                        permission_mode: loom_domain::automation::PermissionMode::Auto,
                        environment: loom_domain::automation::AgentEnvironment::Reuse {
                            environment_id: environment.id,
                        },
                        target_thread_id: None,
                    },
                ),
                origin: loom_domain::automation::AutomationOrigin::Human,
                created_by_thread_id: None,
            },
            loom_relay::now_ms(),
        )
        .unwrap()
}

/// Polls until an automation run reaches a terminal state.
async fn wait_for_automation_run(
    state: &AppState,
    run_id: &loom_domain::AutomationRunId,
) -> loom_domain::AutomationRun {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let run = state.automations.run(run_id).expect("the run is stored");
        if !matches!(
            run.state,
            loom_domain::AutomationRunState::Pending | loom_domain::AutomationRunState::Running
        ) {
            return run;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the automation run never reached a terminal state: {run:?}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn a_scheduled_automation_run_becomes_a_real_turn() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let provider = write_stub(
        dir.path(),
        "automation-agent.sh",
        r#"printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"stub-session","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"scheduled run complete"}}}}'
"#,
    );
    let (url, state) = spawn_server(AppConfig {
        providers: vec![provider.clone()],
        schedule_interval: Duration::ZERO,
        ..AppConfig::default()
    })
    .await;
    let (host_id, worker) =
        enroll_worker(&url, None, None, Some(provider), Duration::from_secs(10)).await;
    assert!(
        eventually(|| state
            .registry
            .host(&host_id)
            .map(|host| host.status == HostStatus::Connected)
            .unwrap_or(false))
        .await
    );

    // A one-shot window that has already arrived: the sweep claims it, and the
    // executor turns it into a turn.
    let now = loom_relay::now_ms();
    let automation = automation_fixture(
        &state,
        &host_id,
        workspace.path(),
        loom_domain::automation::AutomationTrigger::Once {
            run_at: now + 1_000,
        },
    );
    state.sweep_automations(now + 2_000);

    let (runs, _) = state
        .automations
        .runs(
            &automation.project_id.to_string(),
            &automation.id.to_string(),
            10,
            None,
        )
        .unwrap();
    assert_eq!(runs.len(), 1, "the window was claimed into exactly one run");
    assert_eq!(
        runs[0].trigger,
        loom_domain::automation::AutomationRunTrigger::Schedule
    );
    let run = wait_for_automation_run(&state, &runs[0].id).await;

    // The provider really ran: the thread the run created carries the stub's
    // output, and the run says which thread that was.
    assert_eq!(
        run.state,
        loom_domain::AutomationRunState::Succeeded,
        "{run:?}"
    );
    let thread_id = run.thread_id.clone().expect("the run names its thread");
    assert_eq!(
        wait_for_terminal(&state, &thread_id).await,
        ThreadStatus::Idle
    );
    let events = thread_events(&state, &thread_id);
    assert_eq!(terminal_outcome(&events), Some("completed".into()));
    assert_eq!(output_texts(&events), vec!["scheduled run complete"]);
    assert_eq!(
        run.response().status,
        loom_domain::AutomationRunStatus::Succeeded
    );
    assert!(run.finished_at.is_some());
    assert!(
        run.provider_run_id.is_some(),
        "the run records the provider run it became"
    );

    // A one-shot automation does not fire twice.
    state.sweep_automations(now + 3_000);
    let (runs, _) = state
        .automations
        .runs(
            &automation.project_id.to_string(),
            &automation.id.to_string(),
            10,
            None,
        )
        .unwrap();
    assert_eq!(runs.len(), 1);

    worker.abort();
    state.shutdown().unwrap();
}

#[tokio::test]
async fn a_manual_automation_run_dispatches_and_closes_with_its_thread() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let provider = write_stub(
        dir.path(),
        "manual-agent.sh",
        r#"printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"stub-session","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"manual run complete"}}}}'
"#,
    );
    let (url, state) = spawn_server(AppConfig {
        providers: vec![provider.clone()],
        schedule_interval: Duration::ZERO,
        ..AppConfig::default()
    })
    .await;
    let (host_id, worker) =
        enroll_worker(&url, None, None, Some(provider), Duration::from_secs(10)).await;
    assert!(
        eventually(|| state
            .registry
            .host(&host_id)
            .map(|host| host.status == HostStatus::Connected)
            .unwrap_or(false))
        .await
    );

    let automation = automation_fixture(
        &state,
        &host_id,
        workspace.path(),
        loom_domain::automation::AutomationTrigger::Schedule {
            cron: "0 9 * * *".into(),
            timezone: "UTC".into(),
        },
    );

    // The client's request, over the real HTTP surface.
    let addr = url.trim_start_matches("http://").to_string();
    let (status, body) = http(
        &addr,
        "POST",
        &format!(
            "/api/v1/projects/{}/automations/{}/run",
            automation.project_id, automation.id
        ),
        Some(&serde_json::json!({ "idempotencyKey": "manual-1" })),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    let run_id: loom_domain::AutomationRunId = body["run"]["id"].as_str().unwrap().parse().unwrap();
    let run = wait_for_automation_run(&state, &run_id).await;
    assert_eq!(
        run.state,
        loom_domain::AutomationRunState::Succeeded,
        "{run:?}"
    );

    // The response the client saw and the run the history carries agree on the
    // thread, and that thread is the one the provider ran in.
    let thread_id = run.thread_id.clone().expect("the run names its thread");
    let (status, fetched) = http(
        &addr,
        "GET",
        &format!(
            "/api/v1/projects/{}/automations/{}/runs",
            automation.project_id, automation.id
        ),
        None,
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(fetched["runs"][0]["id"], body["run"]["id"]);
    assert_eq!(fetched["runs"][0]["threadId"], thread_id.to_string());
    assert_eq!(fetched["runs"][0]["status"], "succeeded");
    assert_eq!(fetched["runs"][0]["trigger"], "manual");

    let events = thread_events(&state, &thread_id);
    assert_eq!(terminal_outcome(&events), Some("completed".into()));
    assert_eq!(output_texts(&events), vec!["manual run complete"]);

    worker.abort();
    state.shutdown().unwrap();
}

/* ------------------------------------------------------------------ */
/* Automation scripts                                                  */
/* ------------------------------------------------------------------ */

/// Creates a script automation over the HTTP surface and returns its id.
///
/// Scripts need no environment — the machine that owns the automation provides
/// the workspace — so this is the whole fixture: one POST.
async fn script_automation(
    addr: &str,
    script: &str,
    script_file: Option<&str>,
    timeout_ms: u64,
) -> String {
    let mut execution = serde_json::json!({
        "mode": "script",
        "interpreter": "bash",
        "timeoutMs": timeout_ms,
    });
    match script_file {
        Some(path) => execution["scriptFile"] = serde_json::json!(path),
        None => execution["script"] = serde_json::json!(script),
    }
    let (status, created) = http(
        addr,
        "POST",
        &format!("/api/v1/projects/{}/automations", state_project(addr).await),
        Some(&serde_json::json!({
            "name": "script run",
            "trigger": { "triggerType": "schedule", "cron": "0 3 * * *", "timezone": "UTC" },
            "execution": execution,
            "origin": "human"
        })),
    )
    .await;
    assert_eq!(status, 201, "{created}");
    created["id"].as_str().unwrap().to_owned()
}

/// The personal project's id, which every fixture automation belongs to.
///
/// Taken from the sidebar bootstrap rather than `/api/v1/projects`: the
/// personal scope is handed to a client as `personalProject` and is not one of
/// the projects that endpoint lists.
async fn state_project(addr: &str) -> String {
    let (status, bootstrap) = http(addr, "GET", "/api/v1/sidebar-bootstrap", None).await;
    assert_eq!(status, 200, "{bootstrap}");
    bootstrap["personalProject"]["id"]
        .as_str()
        .unwrap()
        .to_owned()
}

/// Waits for an automation run to reach a terminal state, over HTTP.
async fn wait_for_automation_run_over_http(addr: &str, project: &str, run_id: &str) -> Value {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let (status, runs) = http(
            addr,
            "GET",
            &format!("/api/v1/projects/{project}/automations"),
            None,
        )
        .await;
        assert_eq!(status, 200);
        let automation = &runs[0];
        if automation["lastRunStatus"].is_string() || automation["lastError"].is_string() {
            // The history is the authority on the run the client asked for.
            let (status, history) = http(
                addr,
                "GET",
                &format!(
                    "/api/v1/projects/{project}/automations/{}/runs",
                    automation["id"].as_str().unwrap()
                ),
                None,
            )
            .await;
            assert_eq!(status, 200);
            if let Some(run) = history["runs"]
                .as_array()
                .and_then(|runs| runs.iter().find(|run| run["id"] == run_id))
            {
                if run["status"] != "running" {
                    return run.clone();
                }
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the automation run never settled"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn a_script_automation_runs_on_the_worker_and_records_its_output() {
    let dir = tempfile::tempdir().unwrap();
    let (url, state) = spawn_server(AppConfig {
        schedule_interval: Duration::ZERO,
        ..AppConfig::default()
    })
    .await;
    let (host_id, worker) = enroll_worker(
        &url,
        Some(dir.path().join("data")),
        None,
        None,
        Duration::from_secs(10),
    )
    .await;
    assert!(
        eventually(|| state
            .registry
            .host(&host_id)
            .map(|host| host.status == HostStatus::Connected)
            .unwrap_or(false))
        .await
    );

    let addr = url.trim_start_matches("http://").to_string();
    let project = state_project(&addr).await;
    let automation = script_automation(&addr, "echo script-ran; exit 0", None, 5_000).await;
    let (status, queued) = http(
        &addr,
        "POST",
        &format!("/api/v1/projects/{project}/automations/{automation}/run"),
        Some(&serde_json::json!({ "idempotencyKey": "script-1" })),
    )
    .await;
    assert_eq!(status, 201, "{queued}");
    let run_id = queued["run"]["id"].as_str().unwrap().to_owned();

    let run = wait_for_automation_run_over_http(&addr, &project, &run_id).await;
    assert_eq!(run["status"], "succeeded", "{run}");
    assert_eq!(run["runMode"], "script");
    assert_eq!(run["exitCode"], 0);
    assert!(
        run["output"]
            .as_str()
            .is_some_and(|output| output.contains("script-ran")),
        "{run}"
    );

    // The host wrote the inline script somewhere only it knows, and the
    // automation says where, so a user can find the code that ran.
    let (status, stored) = http(
        &addr,
        "GET",
        &format!("/api/v1/projects/{project}/automations/{automation}"),
        None,
    )
    .await;
    assert_eq!(status, 200);
    let stored_path = stored["execution"]["storedScriptPath"]
        .as_str()
        .unwrap_or_else(|| panic!("the script path should be recorded: {stored}"));
    assert!(stored_path.ends_with(".sh"), "{stored_path}");
    assert!(std::path::Path::new(stored_path).exists());
    // …and it is on the worker's machine, under the worker's data directory.
    assert!(
        stored_path.contains("/automation-scripts/"),
        "{stored_path}"
    );

    worker.abort();
    state.shutdown().unwrap();
}

#[tokio::test]
async fn a_non_zero_script_exit_fails_the_run_with_its_code() {
    let dir = tempfile::tempdir().unwrap();
    let (url, state) = spawn_server(AppConfig {
        schedule_interval: Duration::ZERO,
        ..AppConfig::default()
    })
    .await;
    let (host_id, worker) = enroll_worker(
        &url,
        Some(dir.path().join("data")),
        None,
        None,
        Duration::from_secs(10),
    )
    .await;
    assert!(
        eventually(|| state
            .registry
            .host(&host_id)
            .map(|host| host.status == HostStatus::Connected)
            .unwrap_or(false))
        .await
    );

    let addr = url.trim_start_matches("http://").to_string();
    let project = state_project(&addr).await;
    let automation = script_automation(&addr, "echo failing 1>&2; exit 3", None, 5_000).await;
    let (_, queued) = http(
        &addr,
        "POST",
        &format!("/api/v1/projects/{project}/automations/{automation}/run"),
        Some(&serde_json::json!({})),
    )
    .await;
    let run_id = queued["run"]["id"].as_str().unwrap().to_owned();

    let run = wait_for_automation_run_over_http(&addr, &project, &run_id).await;
    assert_eq!(run["status"], "failed", "{run}");
    assert_eq!(run["exitCode"], 3);
    assert_eq!(run["error"], "Script exited with code 3");
    assert!(run["output"]
        .as_str()
        .is_some_and(|o| o.contains("failing")));

    worker.abort();
    state.shutdown().unwrap();
}

#[tokio::test]
async fn a_script_that_outlives_its_timeout_is_killed_and_reported() {
    let dir = tempfile::tempdir().unwrap();
    let (url, state) = spawn_server(AppConfig {
        schedule_interval: Duration::ZERO,
        ..AppConfig::default()
    })
    .await;
    let (host_id, worker) = enroll_worker(
        &url,
        Some(dir.path().join("data")),
        None,
        None,
        Duration::from_secs(10),
    )
    .await;
    assert!(
        eventually(|| state
            .registry
            .host(&host_id)
            .map(|host| host.status == HostStatus::Connected)
            .unwrap_or(false))
        .await
    );

    let addr = url.trim_start_matches("http://").to_string();
    let project = state_project(&addr).await;
    let automation = script_automation(&addr, "sleep 30", None, 500).await;
    let (_, queued) = http(
        &addr,
        "POST",
        &format!("/api/v1/projects/{project}/automations/{automation}/run"),
        Some(&serde_json::json!({})),
    )
    .await;
    let run_id = queued["run"]["id"].as_str().unwrap().to_owned();

    let run = wait_for_automation_run_over_http(&addr, &project, &run_id).await;
    assert_eq!(run["status"], "failed", "{run}");
    assert_eq!(run["error"], "Script timed out");
    assert!(run["finishedAt"].is_u64());

    worker.abort();
    state.shutdown().unwrap();
}

#[tokio::test]
async fn pausing_an_automation_stops_its_running_script() {
    let dir = tempfile::tempdir().unwrap();
    let (url, state) = spawn_server(AppConfig {
        schedule_interval: Duration::ZERO,
        ..AppConfig::default()
    })
    .await;
    let (host_id, worker) = enroll_worker(
        &url,
        Some(dir.path().join("data")),
        None,
        None,
        Duration::from_secs(10),
    )
    .await;
    assert!(
        eventually(|| state
            .registry
            .host(&host_id)
            .map(|host| host.status == HostStatus::Connected)
            .unwrap_or(false))
        .await
    );

    let addr = url.trim_start_matches("http://").to_string();
    let project = state_project(&addr).await;
    let automation = script_automation(&addr, "sleep 30", None, 30_000).await;
    let (_, queued) = http(
        &addr,
        "POST",
        &format!("/api/v1/projects/{project}/automations/{automation}/run"),
        Some(&serde_json::json!({})),
    )
    .await;
    let run_id = queued["run"]["id"].as_str().unwrap().to_owned();

    // Wait until the machine is actually running it, then pause the automation.
    assert!(
        eventually(|| state.automations.running_script_runs().len() == 1).await,
        "the script should reach the running state"
    );
    let (status, paused) = http(
        &addr,
        "POST",
        &format!("/api/v1/projects/{project}/automations/{automation}/pause"),
        None,
    )
    .await;
    assert_eq!(status, 200, "{paused}");

    let run = wait_for_automation_run_over_http(&addr, &project, &run_id).await;
    // The contract has one "not run" status, so a cancel surfaces as `skipped`
    // with the reason; the run's own state is the cancelled one.
    assert_eq!(run["status"], "skipped", "{run}");
    assert!(run["skipReason"]
        .as_str()
        .is_some_and(|reason| reason.contains("paused")));
    assert!(run["finishedAt"].is_u64());
    let stored = state
        .automations
        .run(&run_id.parse().unwrap())
        .expect("stored");
    assert_eq!(
        stored.state,
        loom_domain::automation::AutomationRunState::Cancelled
    );

    worker.abort();
    state.shutdown().unwrap();
}

#[tokio::test]
async fn a_script_path_outside_the_workspace_is_refused_by_the_host() {
    let dir = tempfile::tempdir().unwrap();
    let (url, state) = spawn_server(AppConfig {
        schedule_interval: Duration::ZERO,
        ..AppConfig::default()
    })
    .await;
    let (host_id, worker) = enroll_worker(
        &url,
        Some(dir.path().join("data")),
        None,
        None,
        Duration::from_secs(10),
    )
    .await;
    assert!(
        eventually(|| state
            .registry
            .host(&host_id)
            .map(|host| host.status == HostStatus::Connected)
            .unwrap_or(false))
        .await
    );

    let addr = url.trim_start_matches("http://").to_string();
    let project = state_project(&addr).await;
    // A traversal out of the script directory: the host refuses it and the run
    // carries the reason.
    let automation = script_automation(&addr, "", Some("../../etc/passwd"), 5_000).await;
    let (_, queued) = http(
        &addr,
        "POST",
        &format!("/api/v1/projects/{project}/automations/{automation}/run"),
        Some(&serde_json::json!({})),
    )
    .await;
    let run_id = queued["run"]["id"].as_str().unwrap().to_owned();

    let run = wait_for_automation_run_over_http(&addr, &project, &run_id).await;
    assert_eq!(run["status"], "failed", "{run}");
    assert!(
        run["error"]
            .as_str()
            .is_some_and(|error| error.contains("invalid relative path")),
        "{run}"
    );

    worker.abort();
    state.shutdown().unwrap();
}
