//! Worker self-update, end to end, with the real worker binary.
//!
//! The acceptance scenario in `docs/upgrades.md` is:
//!
//! > protocol mismatch → update → reconnect succeeds → the run is handled
//! > correctly
//!
//! and this file walks it with real processes rather than mocks of the parts
//! that matter:
//!
//! 1. A **fake server** that speaks a *newer* protocol version than the worker
//!    build and hosts a real `loom-worker` binary. It is a faithful stand-in for
//!    the one property under test — "a server announces a protocol this worker
//!    cannot speak, and offers the matching binary" — and nothing else.
//! 2. The **real worker process**, started with the same flags systemd uses,
//!    connects, is refused, fetches the artifact, verifies its SHA-256,
//!    installs it **over its own executable**, and exits **0** so
//!    `Restart=always` starts the new file.
//! 3. The test then does what the supervisor would: it runs the **installed
//!    file** against a **real loom server**, and drives a real provider turn to
//!    a terminal state. The reconnect and the run are the production path.
//!
//! # How "the file was replaced" is observed
//!
//! Self-update replaces the running executable, so a test cannot copy an
//! arbitrary path over the source binary. It installs a *stale variant* of the
//! real worker — the same executable with a marker appended, which the ELF
//! loader ignores — starts that, and asserts the file afterwards is the
//! unmodified release bytes the server served. A marker check would prove less:
//! this compares whole files.
//!
//! The tests are `#[ignore]`d like the real-Pi test, because they execute real
//! binaries and a fake network endpoint; CI runs them explicitly.

use std::path::{Path, PathBuf};
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use loom_domain::{MessageRole, ThreadStatus};
use loom_provider_protocol::ProviderSpec;
use loom_server::artifacts::{sha256_hex, DIGEST_HEADER};
use loom_server::http::router as server_router;
use loom_server::state::{AppConfig, AppState};
use loom_server::PROTOCOL_VERSION;
use loom_worker::{Worker, WorkerConfig};
use serde_json::json;

/// The real worker binary, built by cargo for this integration test.
/// The one installed binary, driven in its worker role.
///
/// `cargo` sets this for the crate whose binary is under test, which is why
/// this file lives beside the binary it starts: the worker has no artifact of
/// its own any more.
const BINARY: &str = env!("CARGO_BIN_EXE_loom");
const WORKER_ROLE: &str = "worker";

/// The marker appended to make the stale variant distinguishable.
const STALE_MARKER: &[u8] = b"\n# a stale worker, awaiting self-update\n";

/// The newer protocol the fake server claims to speak.
fn future_protocol() -> u32 {
    PROTOCOL_VERSION + 1
}

/// The fake newer server: a WebSocket that refuses this worker's protocol, plus
/// the two install routes serving a real binary.
#[derive(Clone)]
struct FakeServer {
    worker_bytes: Vec<u8>,
    digest: String,
    protocol_version: u32,
    /// Every artifact request seen, as `(sent_if_none_match, answered_304)`.
    artifact_requests: std::sync::Arc<std::sync::Mutex<Vec<(bool, bool)>>>,
}

async fn spawn_fake_server(bytes: Vec<u8>) -> (String, FakeServer) {
    let state = FakeServer {
        digest: sha256_hex(&bytes),
        worker_bytes: bytes,
        protocol_version: future_protocol(),
        artifact_requests: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
    };
    let app = Router::new()
        .route("/internal/ws", get(fake_ws))
        .route("/install/version", get(fake_version))
        .route("/install/loom-worker", get(fake_artifact))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{}:{}", addr.ip(), addr.port()), state)
}

/// The `hello` frame a newer server sends: accepted as the first frame, then
/// refused by the worker before it enrolls.
async fn fake_ws(upgrade: WebSocketUpgrade, State(state): State<FakeServer>) -> Response {
    upgrade.on_upgrade(move |socket| fake_ws_session(socket, state))
}

async fn fake_ws_session(mut socket: WebSocket, state: FakeServer) {
    // `axum`'s `WebSocket` has an inherent `send`, so no `SinkExt` is needed.
    let hello = json!({
        "type": "hello",
        "protocol_version": state.protocol_version,
    });
    let _ = socket.send(Message::Text(hello.to_string().into())).await;
    // The worker drops the socket itself on the refusal; this only keeps the
    // task alive long enough for the frame to flush.
    let _ = socket.recv().await;
}

async fn fake_version(State(state): State<FakeServer>) -> Json<serde_json::Value> {
    Json(json!({
        "version": "9.9.9",
        "protocolVersion": state.protocol_version,
    }))
}

#[derive(serde::Deserialize)]
struct ArtifactQuery {
    #[allow(dead_code)]
    target: Option<String>,
}

async fn fake_artifact(
    State(state): State<FakeServer>,
    Query(_query): Query<ArtifactQuery>,
    headers: axum::http::HeaderMap,
) -> Response {
    let if_none_match = headers
        .get(axum::http::header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| {
            value
                .trim()
                .trim_matches('"')
                .strip_prefix("sha256-")
                .map(str::to_owned)
        });
    let matching = if_none_match.as_deref() == Some(state.digest.as_str());
    state
        .artifact_requests
        .lock()
        .unwrap()
        .push((if_none_match.is_some(), matching));

    let mut response_headers = axum::http::HeaderMap::new();
    response_headers.insert(
        axum::http::HeaderName::from_static(DIGEST_HEADER),
        state.digest.parse().unwrap(),
    );
    response_headers.insert(
        axum::http::header::ETAG,
        format!("\"sha256-{}\"", state.digest).parse().unwrap(),
    );
    if matching {
        return (axum::http::StatusCode::NOT_MODIFIED, response_headers).into_response();
    }
    response_headers.insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/octet-stream"),
    );
    (
        axum::http::StatusCode::OK,
        response_headers,
        state.worker_bytes.clone(),
    )
        .into_response()
}

/// Installs the stale variant of the real worker at `path` and makes it
/// executable. Appended bytes are invisible to the ELF loader, so this runs.
fn install_stale_variant(path: &Path) {
    let mut stale = std::fs::read(BINARY).expect("the built worker binary");
    stale.extend_from_slice(STALE_MARKER);
    std::fs::write(path, &stale).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

/// Starts a real control plane on an ephemeral port.
async fn spawn_server(config: AppConfig) -> (String, AppState) {
    let state = AppState::build(config).unwrap();
    let app = server_router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{}:{}", addr.ip(), addr.port()), state)
}

/// Writes an executable ACP agent stub and returns its spec.
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

async fn eventually(mut predicate: impl FnMut() -> bool) -> bool {
    for _ in 0..800 {
        if predicate() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    predicate()
}

/// Runs a worker binary to completion, returning `(status, stderr)`.
async fn run_to_completion(binary: &Path, args: &[&str]) -> (std::process::ExitStatus, String) {
    let output = tokio::process::Command::new(binary)
        .arg(WORKER_ROLE)
        .args(args)
        .output()
        .await
        .expect("the worker binary must be runnable");
    (
        output.status,
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// Spawns a worker that should stay up, returning it and its stderr log task.
async fn spawn_worker(
    binary: &Path,
    server_url: &str,
    state_path: &Path,
) -> (tokio::process::Child, tokio::task::JoinHandle<String>) {
    std::fs::create_dir_all(state_path.parent().unwrap()).unwrap();
    let mut child = tokio::process::Command::new(binary)
        .arg(WORKER_ROLE)
        .arg("--server-url")
        .arg(server_url)
        .arg("--state")
        .arg(state_path)
        .arg("--heartbeat-ms")
        .arg("50")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("the worker binary must be runnable");
    let stderr = child.stderr.take().unwrap();
    let log = tokio::spawn(async move {
        use tokio::io::AsyncReadExt;
        let mut reader = tokio::io::BufReader::new(stderr);
        let mut text = String::new();
        let _ = reader.read_to_string(&mut text).await;
        text
    });
    (child, log)
}

/// The acceptance scenario, in one process:
/// mismatch → update → reconnect → the run is handled correctly.
#[tokio::test]
async fn a_v3_worker_fails_fast_when_deployed_before_a_v2_server() {
    let app = Router::new().route("/ws", get(|| async { "legacy websocket" }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let config = WorkerConfig::new(format!("http://{addr}"), "new-worker").without_discovery();
    let result = tokio::time::timeout(Duration::from_secs(2), Worker::connect(config))
        .await
        .expect("a missing internal endpoint must fail rather than hang");
    let error = match result {
        Err(error) => error,
        Ok(_) => panic!("a v3 worker must not enroll against a v2-only server"),
    };
    let detail = error.to_string();
    assert!(
        detail.contains("404") || detail.contains("HTTP error"),
        "unexpected diagnostic: {detail}"
    );
}

#[tokio::test]
#[ignore = "runs a real worker binary and a fake network endpoint"]
async fn a_protocol_mismatch_updates_the_worker_and_the_new_binary_runs_a_turn() {
    let staging = tempfile::tempdir().unwrap();
    let install_dir = tempfile::tempdir().unwrap();
    let install_path: PathBuf = install_dir.path().join("loom-worker");
    install_stale_variant(&install_path);
    let stale_bytes = std::fs::read(&install_path).unwrap();

    // The bytes the server hosts are the real worker this repository built, so
    // the "new binary" the supervisor starts is a production executable.
    let real_binary = std::fs::read(BINARY).expect("the built worker binary");
    let (fake_url, fake) = spawn_fake_server(real_binary.clone()).await;
    assert_eq!(fake.protocol_version, PROTOCOL_VERSION + 1);

    let state_path = install_dir.path().join("state").join("host-id");

    // 1. The stale worker connects to a server that speaks a newer protocol. It
    //    is refused, fetches the matching binary, installs it over itself, and
    //    exits 0 for the supervisor.
    let (status, stderr) = run_to_completion(
        &install_path,
        &[
            "--server-url",
            &fake_url,
            "--state",
            state_path.to_str().unwrap(),
            "--heartbeat-ms",
            "50",
        ],
    )
    .await;
    assert!(
        status.success(),
        "an update must exit 0 so Restart=always starts the new file; stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("exiting for a self-update"),
        "the exit must say why; stderr:\n{stderr}"
    );
    assert!(
        stderr.contains(&format!("protocol version {}", PROTOCOL_VERSION + 1)),
        "the refusal must name the server's protocol version; stderr:\n{stderr}"
    );

    // 2. The file is now the server's artifact, byte for byte, executable, and
    //    no longer the stale variant.
    let installed = std::fs::read(&install_path).expect("the installed binary");
    assert_ne!(
        installed, stale_bytes,
        "the stale file must have been replaced"
    );
    assert_eq!(
        sha256_hex(&installed),
        fake.digest,
        "the installed file must be the downloaded artifact"
    );
    assert_eq!(installed, real_binary);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&install_path)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o777,
            0o755,
            "the supervisor must be able to execute it"
        );
    }

    // The first fetch carried no validator.
    assert_eq!(
        fake.artifact_requests.lock().unwrap().as_slice(),
        &[(false, false)],
        "the first fetch is unconditional"
    );

    // The digest was recorded, so the next attempt can be conditional.
    let recorded = std::fs::read_to_string(
        install_dir
            .path()
            .join("state")
            .join("host-artifact.sha256"),
    )
    .unwrap();
    assert_eq!(recorded.trim(), fake.digest);

    // 3. What the supervisor does next: start the installed file against a real
    //    control plane. This is the "reconnect succeeds" half.
    let provider = write_stub(
        staging.path(),
        "provider.sh",
        r#"printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"stub-session","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"updated and running"}}}}'
"#,
    );
    let workspace = tempfile::tempdir().unwrap();
    // `--no-auto-update` here: the real server's protocol matches, so this only
    // documents that the update half is finished with this binary.
    let (real_url, server_state) = spawn_server(AppConfig {
        providers: vec![provider],
        ..AppConfig::default()
    })
    .await;

    // A fresh state path: the file the update run used is the misconfigured
    // server's, and reusing it would only test the identity path.
    let child_state = install_dir.path().join("state2").join("host-id");
    let (mut child, log) = spawn_worker(&install_path, &real_url, &child_state).await;

    assert!(
        eventually(|| !server_state.registry.hosts().is_empty()).await,
        "the reinstalled worker must enrol against the real server"
    );
    let host = server_state.registry.hosts().into_iter().next().unwrap();

    // 4. A real provider turn, dispatched through the relay to that worker.
    let (environment, _) = server_state
        .registry
        .create_environment(
            Some(server_state.registry.personal_project_id()),
            host.id.clone(),
            loom_domain::EnvironmentKind::Unmanaged,
            Some(workspace.path().to_string_lossy().into_owned()),
            loom_relay::now_ms(),
        )
        .unwrap();
    let (thread, created) = server_state
        .registry
        .create_thread(
            Some(server_state.registry.personal_project_id()),
            Some("after an update".into()),
            Some(environment.id),
            loom_relay::now_ms(),
        )
        .unwrap();
    server_state.publish_domain_event(&created).unwrap();
    for event in server_state
        .registry
        .post_message(
            &thread.id,
            MessageRole::User,
            "say something".to_owned(),
            loom_relay::now_ms(),
        )
        .unwrap()
    {
        server_state.publish_domain_event(&event).unwrap();
    }
    let thread = server_state.registry.thread(&thread.id).unwrap();
    server_state.dispatch_thread(&thread, "say something");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let status = server_state.registry.thread(&thread.id).unwrap().status;
        if matches!(
            status,
            ThreadStatus::Idle | ThreadStatus::Error | ThreadStatus::Archived
        ) {
            assert_eq!(
                status,
                ThreadStatus::Idle,
                "the reinstalled worker must complete the turn"
            );
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the thread never left `working`"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // The run's frames are in the log and replayable, exactly as for any other
    // turn: an update did not create a special case.
    let scope = loom_relay::Scope::Thread(thread.id.to_string());
    let frames: Vec<serde_json::Value> = server_state
        .relay
        .replay_scope(&scope, 500)
        .unwrap()
        .into_iter()
        .filter_map(|envelope| {
            let frame: serde_json::Value = serde_json::from_slice(&envelope.payload).ok()?;
            let payload = frame["payload"].as_str()?;
            serde_json::from_str(payload).ok()
        })
        .collect();
    let deltas: Vec<&str> = frames
        .iter()
        .filter(|event| event["type"] == "thread_run_event")
        .filter(|event| event["event"]["type"] == "item/agentMessage/delta")
        .filter_map(|event| event["event"]["delta"].as_str())
        .collect();
    assert_eq!(deltas, vec!["updated and running"]);

    let _ = child.kill().await;
    let log = log.await.unwrap_or_default();
    assert!(
        log.contains("enrolled as"),
        "the reinstalled worker should have enrolled; stderr:\n{log}"
    );
    server_state.shutdown();
}

/// A second attempt against an unchanged server must be a conditional request
/// answered `304`, and it must **still** exit for the restart.
#[tokio::test]
#[ignore = "runs a real worker binary and a fake network endpoint"]
async fn a_second_update_against_an_unchanged_artifact_is_a_304_and_a_restart() {
    let install_dir = tempfile::tempdir().unwrap();
    let install_path: PathBuf = install_dir.path().join("loom-worker");
    install_stale_variant(&install_path);
    let real_binary = std::fs::read(BINARY).expect("the built worker binary");
    let (fake_url, fake) = spawn_fake_server(real_binary.clone()).await;
    let state_path = install_dir.path().join("state").join("host-id");

    // First run: unconditional fetch, install, exit for the restart.
    let (status, stderr) = run_to_completion(
        &install_path,
        &[
            "--server-url",
            &fake_url,
            "--state",
            state_path.to_str().unwrap(),
        ],
    )
    .await;
    assert!(status.success(), "{stderr}");
    assert_eq!(std::fs::read(&install_path).unwrap(), real_binary);

    // Second run: the digest is remembered and offered as a validator.
    let (status, stderr) = run_to_completion(
        &install_path,
        &[
            "--server-url",
            &fake_url,
            "--state",
            state_path.to_str().unwrap(),
        ],
    )
    .await;
    assert!(status.success(), "{stderr}");
    let requests = fake.artifact_requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 2, "two runs, two fetches: {requests:?}");
    assert_eq!(requests[0], (false, false), "the first is unconditional");
    assert_eq!(
        requests[1],
        (true, true),
        "the second offers the installed digest and is answered 304"
    );
    assert!(
        stderr.contains("already installed"),
        "a 304 must still end in a restart, not a silent no-op; stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("exiting for a self-update"),
        "a 304 must exit for the supervisor; stderr:\n{stderr}"
    );
    assert_eq!(
        std::fs::read_to_string(
            install_dir
                .path()
                .join("state")
                .join("host-artifact.sha256")
        )
        .unwrap()
        .trim(),
        fake.digest
    );
}

/// The operator switch: with self-update off, a mismatched server is refused and
/// retried, never fetched, and the process keeps running its current binary.
#[tokio::test]
#[ignore = "runs a real worker binary and a fake network endpoint"]
async fn a_worker_with_self_update_disabled_never_fetches_and_keeps_running() {
    let install_dir = tempfile::tempdir().unwrap();
    let install_path: PathBuf = install_dir.path().join("loom-worker");
    install_stale_variant(&install_path);
    let stale = std::fs::read(&install_path).unwrap();
    let real_binary = std::fs::read(BINARY).expect("the built worker binary");
    let (fake_url, fake) = spawn_fake_server(real_binary).await;
    let state_path = install_dir.path().join("state").join("host-id");

    // A real worker process with self-update disabled. It must not exit: a
    // refused connection is retried, and the reason is logged.
    let mut child = tokio::process::Command::new(&install_path)
        .arg(WORKER_ROLE)
        .arg("--server-url")
        .arg(&fake_url)
        .arg("--state")
        .arg(&state_path)
        .arg("--no-auto-update")
        .arg("--heartbeat-ms")
        .arg("50")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("the worker binary must be runnable");

    // Give it well past the first reconnect delay (1 s) to prove it stays up.
    tokio::time::sleep(Duration::from_millis(1800)).await;
    assert!(
        child.try_wait().unwrap().is_none(),
        "a disabled updater must not exit; it retries"
    );
    assert!(
        fake.artifact_requests.lock().unwrap().is_empty(),
        "a disabled updater must never fetch"
    );
    assert_eq!(
        std::fs::read(&install_path).unwrap(),
        stale,
        "a disabled updater must not touch the binary"
    );

    child.kill().await.unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let mut log = String::new();
    use tokio::io::AsyncReadExt;
    let _ = stderr.read_to_string(&mut log).await;
    assert!(
        log.contains("disabled"),
        "the reason must be logged; stderr:\n{log}"
    );
    assert!(
        log.contains("protocol version"),
        "the refusal must be logged; stderr:\n{log}"
    );
}
