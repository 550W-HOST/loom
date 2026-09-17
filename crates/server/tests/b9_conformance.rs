//! B9 conformance: workspace file operations and terminal sessions.
//!
//! Every route in this batch is a request to the machine that owns the file or
//! the process. These tests therefore use a **scripted host**: a real WebSocket
//! connection that enrolls as a host, receives whatever the control plane
//! published to its room, and answers with the frame a real daemon would send.
//! That is the property the batch must prove — a file read never touches the
//! server's own disk, and a terminal is never a server-side abstraction — and a
//! scripted host can assert exactly which operation it was asked to perform.
//!
//! The filesystem and PTY semantics themselves (containment, base64, mode bits,
//! optimistic-concurrency conflicts, output cursors) are covered against the
//! real implementations in `crates/daemon`, because that is where the file and
//! the process actually are.
//!
//! Every successful body is validated against the embedded bb contract, and
//! every JSON request body is asserted against the contract's request schema —
//! including the counter-examples the acceptance criteria require.

use std::future::Future;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use loom_contract::shared;
use loom_domain::{EnvironmentKind, HostId, ProjectKind};
use loom_provider_protocol::{
    HostFileContent, HostFileEncoding, HostFileEntry, HostFileOperation, HostFileOutcome,
    HostFileReport, HostPathKind, TerminalOperation, TerminalOutcome, TerminalReport,
    TerminalSession, TerminalStatus,
};
use loom_server::http::router;
use loom_server::state::{AppConfig, AppState};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

const TIMEOUT: Duration = Duration::from_secs(10);
const UNKNOWN_HOST: &str = "host_01M27Y6Q0J8V4W2C7K5N3P1R9Z";
const WORKSPACE: &str = "/srv/b9";

async fn spawn_server() -> (String, AppState) {
    let state = AppState::build(AppConfig::default()).unwrap();
    let app = router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("{}:{}", addr.ip(), addr.port()), state)
}

/// One HTTP response, with the body decoded when it is JSON.
struct Response {
    status: u16,
    body: Value,
}

async fn request(addr: &str, method: &str, path: &str, body: Option<Value>) -> Response {
    let payload = body.as_ref().map(Value::to_string);
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let head = match payload.as_ref() {
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
    tokio::time::timeout(TIMEOUT, stream.read_to_end(&mut raw))
        .await
        .expect("HTTP response timed out")
        .unwrap();
    let separator = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("response had no header separator")
        + 4;
    let head = String::from_utf8_lossy(&raw[..separator]).into_owned();
    let bytes = raw[separator..].to_vec();
    let status = head
        .split(' ')
        .nth(1)
        .and_then(|code| code.parse().ok())
        .expect("no HTTP status line");
    let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    Response { status, body }
}

#[track_caller]
fn assert_response(route_id: &str, status: u16, body: &Value) {
    let contract = shared();
    let route = contract
        .route_by_id(route_id)
        .unwrap_or_else(|| panic!("the contract has no route {route_id}"));
    let violations = contract.validate_response(route, status, body);
    assert!(
        violations.is_empty(),
        "{route_id} answers {status} outside its declared schema: {}\n{body}",
        loom_contract::describe(&violations)
    );
}

#[track_caller]
fn assert_error(status: u16, body: &Value) {
    let contract = shared();
    let violations = contract.validate_error_body(body);
    assert!(
        violations.is_empty(),
        "error body is outside apiErrorSchema: {}\n{body}",
        loom_contract::describe(&violations)
    );
    let code = body["code"]
        .as_str()
        .unwrap_or_else(|| panic!("error body has no code: {body}"));
    let declared = contract.error_statuses(code);
    assert!(
        declared.contains(&u64::from(status)),
        "code {code:?} is not declared at {status}; the contract declares it at {declared:?}"
    );
}

#[track_caller]
fn assert_status_and_error(response: &Response, status: u16, code: &str) {
    assert_eq!(
        response.status, status,
        "expected {status} ({code}), got {} with {:?}",
        response.status, response.body
    );
    assert_error(response.status, &response.body);
    assert_eq!(response.body["code"], code);
}

/// Asserts the uniform error shape and that the code is one the contract lists.
///
/// Used for the middleware's `422`, which precedes any handler: whether that
/// status is the one the code declares is W-554's existing decision, so this
/// checks the body and the code's membership, not the pair.
#[track_caller]
fn assert_middleware_error(body: &Value) {
    let contract = shared();
    let violations = contract.validate_error_body(body);
    assert!(
        violations.is_empty(),
        "error body is outside apiErrorSchema: {}\n{body}",
        loom_contract::describe(&violations)
    );
    let code = body["code"]
        .as_str()
        .unwrap_or_else(|| panic!("error body has no code: {body}"));
    assert!(
        !contract.error_statuses(code).is_empty(),
        "error code {code:?} is not one of the contract's codes: {body}"
    );
}

/* ------------------------------------------------------------------ */
/* Fixture                                                             */
/* ------------------------------------------------------------------ */

enum HostRequest {
    File(HostFileOperation),
    Terminal(TerminalOperation),
}

struct Fixture {
    addr: String,
    state: AppState,
    host_id: HostId,
    /// Requests the scripted host received, in order.
    requests: mpsc::UnboundedReceiver<HostRequest>,
    /// Scripted file answers, consumed in order.
    file_script: mpsc::UnboundedSender<HostFileOutcome>,
    /// Scripted terminal answers, consumed in order.
    terminal_script: mpsc::UnboundedSender<TerminalOutcome>,
    host: tokio::task::JoinHandle<()>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.state.shutdown();
        self.host.abort();
    }
}

impl Fixture {
    async fn next_request(&mut self) -> HostRequest {
        tokio::time::timeout(TIMEOUT, self.requests.recv())
            .await
            .expect("the control plane never asked the host")
            .expect("the host connection closed")
    }

    async fn next_file_request(&mut self) -> HostFileOperation {
        match self.next_request().await {
            HostRequest::File(operation) => operation,
            HostRequest::Terminal(operation) => {
                panic!("expected a file request, got a terminal operation: {operation:?}")
            }
        }
    }

    async fn next_terminal_request(&mut self) -> TerminalOperation {
        match self.next_request().await {
            HostRequest::Terminal(operation) => operation,
            HostRequest::File(operation) => {
                panic!("expected a terminal request, got a file operation: {operation:?}")
            }
        }
    }

    fn answer_file(&self, outcome: HostFileOutcome) {
        self.file_script.send(outcome).unwrap();
    }

    fn answer_terminal(&self, outcome: TerminalOutcome) {
        self.terminal_script.send(outcome).unwrap();
    }

    fn post(&self, path: &str, body: Option<Value>) -> impl Future<Output = Response> + '_ {
        let addr = self.addr.clone();
        let path = path.to_owned();
        async move { request(&addr, "POST", &path, body).await }
    }

    fn get<'a>(&'a self, path: &'a str) -> impl Future<Output = Response> + 'a {
        request(&self.addr, "GET", path, None)
    }

    fn patch(&self, path: &str, body: Value) -> impl Future<Output = Response> + '_ {
        let addr = self.addr.clone();
        let path = path.to_owned();
        async move { request(&addr, "PATCH", &path, Some(body)).await }
    }

    fn delete(&self, path: &str, body: Option<Value>) -> impl Future<Output = Response> + '_ {
        let addr = self.addr.clone();
        let path = path.to_owned();
        async move { request(&addr, "DELETE", &path, body).await }
    }
}

async fn fixture() -> Fixture {
    let (addr, state) = spawn_server().await;

    let (requests_tx, requests) = mpsc::unbounded_channel::<HostRequest>();
    let (file_script_tx, file_script_rx) = mpsc::unbounded_channel::<HostFileOutcome>();
    let (terminal_script_tx, terminal_script_rx) = mpsc::unbounded_channel::<TerminalOutcome>();
    let host_id = spawn_scripted_host(&addr, requests_tx, file_script_rx, terminal_script_rx).await;

    Fixture {
        addr,
        state,
        host_id,
        requests,
        file_script: file_script_tx,
        terminal_script: terminal_script_tx,
        host: tokio::spawn(async {}),
    }
}

/// Enrolls a scripted host over a real socket and answers what it is asked.
async fn spawn_scripted_host(
    addr: &str,
    requests: mpsc::UnboundedSender<HostRequest>,
    mut file_script: mpsc::UnboundedReceiver<HostFileOutcome>,
    mut terminal_script: mpsc::UnboundedReceiver<TerminalOutcome>,
) -> HostId {
    let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/internal/ws"))
        .await
        .unwrap();
    let welcome = recv_value(&mut socket).await;
    assert_eq!(welcome["type"], "hello");

    socket
        .send(tokio_tungstenite::tungstenite::Message::Text(
            json!({
                "type": "enroll_host",
                "name": "b9-scripted",
                "data_dir": "/var/lib/loom",
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();
    let enrolled = recv_value(&mut socket).await;
    assert_eq!(enrolled["type"], "host_enrolled", "{enrolled}");
    let host_id: HostId = enrolled["host"]["id"].as_str().unwrap().parse().unwrap();
    let task_host_id = host_id.clone();

    socket
        .send(tokio_tungstenite::tungstenite::Message::Text(
            json!({
                "type": "subscribe",
                "scope": { "kind": "host", "id": host_id.to_string() },
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();
    let subscribed = recv_value(&mut socket).await;
    assert_eq!(subscribed["type"], "subscribed", "{subscribed}");

    tokio::spawn(async move {
        let mut last_file: Option<HostFileOutcome> = None;
        let mut last_terminal: Option<TerminalOutcome> = None;
        loop {
            let frame = recv_value(&mut socket).await;
            if frame["type"] != "event" {
                continue;
            }
            let payload: Value = serde_json::from_str(frame["payload"].as_str().unwrap()).unwrap();

            if let Ok(request) =
                serde_json::from_value::<loom_provider_protocol::HostFileRequest>(payload.clone())
            {
                let _ = requests.send(HostRequest::File(request.operation));
                let outcome = match file_script.try_recv() {
                    Ok(outcome) => {
                        last_file = Some(outcome.clone());
                        outcome
                    }
                    Err(_) => last_file.clone().unwrap_or(HostFileOutcome::Failed {
                        code: "not_found".into(),
                        message: "the scripted host has no answer".into(),
                    }),
                };
                let report = HostFileReport {
                    host_id: task_host_id.clone(),
                    request_id: request.request_id,
                    outcome,
                };
                let _ = socket
                    .send(tokio_tungstenite::tungstenite::Message::Text(
                        json!({ "type": "host_file_report", "report": report })
                            .to_string()
                            .into(),
                    ))
                    .await;
                continue;
            }

            if let Ok(request) =
                serde_json::from_value::<loom_provider_protocol::TerminalRequest>(payload.clone())
            {
                let _ = requests.send(HostRequest::Terminal(request.operation.clone()));
                let outcome = match terminal_script.try_recv() {
                    Ok(outcome) => {
                        last_terminal = Some(outcome.clone());
                        outcome
                    }
                    Err(_) => last_terminal.clone().unwrap_or(TerminalOutcome::Failed {
                        code: "terminal_not_found".into(),
                        message: "the scripted host has no answer".into(),
                    }),
                };
                let report = TerminalReport {
                    host_id: task_host_id.clone(),
                    request_id: request.request_id,
                    outcome,
                };
                let _ = socket
                    .send(tokio_tungstenite::tungstenite::Message::Text(
                        json!({ "type": "terminal_report", "report": report })
                            .to_string()
                            .into(),
                    ))
                    .await;
            }
        }
    });
    host_id
}

async fn recv_value<S>(socket: &mut tokio_tungstenite::WebSocketStream<S>) -> Value
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let message = tokio::time::timeout(TIMEOUT, socket.next())
        .await
        .expect("WebSocket response timed out")
        .expect("WebSocket closed")
        .unwrap();
    match message {
        tokio_tungstenite::tungstenite::Message::Text(text) => {
            serde_json::from_str(text.as_str()).unwrap()
        }
        tokio_tungstenite::tungstenite::Message::Binary(bytes) => {
            serde_json::from_slice(&bytes).unwrap()
        }
        other => panic!("expected a JSON frame, got {other:?}"),
    }
}

fn file_entry(path: &str, kind: HostPathKind) -> HostFileEntry {
    let name = path.rsplit('/').next().unwrap_or(path).to_owned();
    HostFileEntry {
        path: path.to_owned(),
        name,
        kind,
        score: 0.0,
        positions: Vec::new(),
    }
}

fn session(id: &str, host_id: &HostId, status: TerminalStatus) -> TerminalSession {
    TerminalSession {
        id: id.to_owned(),
        thread_id: None,
        environment_id: None,
        host_id: host_id.clone(),
        title: "shell".into(),
        initial_cwd: WORKSPACE.into(),
        cols: 80,
        rows: 24,
        status,
        exit_code: None,
        close_reason: None,
        created_at_ms: 1,
        updated_at_ms: 1,
        last_user_input_at_ms: None,
        next_seq: 0,
    }
}

/* ------------------------------------------------------------------ */
/* Request shape                                                       */
/* ------------------------------------------------------------------ */

#[test]
// `rustfmt::skip` keeps each `validate_request_by_id("...")` call on one line.
// `scripts/check-api-coverage.mjs` asserts that every implemented body-bearing
// route has a request-conformance test by looking for exactly that substring, so
// wrapping a call across lines would silently drop the route from the check.
#[rustfmt::skip]
fn b9_json_requests_match_the_contract() {
    assert!(shared().validate_request_by_id("files.list", &json!({ "path": "/srv" })).is_empty());
    assert!(shared().validate_request_by_id("files.listPaths", &json!({ "path": "/srv", "includeFiles": true, "includeDirectories": false })).is_empty());
    assert!(shared().validate_request_by_id("files.mkdir", &json!({ "path": "src" })).is_empty());
    assert!(shared().validate_request_by_id("files.move", &json!({ "sourcePath": "a", "destinationPath": "b" })).is_empty());
    assert!(shared().validate_request_by_id("files.read", &json!({ "path": "src/lib.rs" })).is_empty());
    assert!(shared().validate_request_by_id("files.remove", &json!({ "path": "src/old.rs" })).is_empty());
    assert!(shared().validate_request_by_id("files.write", &json!({ "path": "a.txt", "content": "hi" })).is_empty());
    assert!(shared().validate_request_by_id("terminals.create", &json!({ "cols": 80, "rows": 24, "target": { "kind": "host_path", "hostId": "host_1", "cwd": null } })).is_empty());
    assert!(shared().validate_request_by_id("terminals.input", &json!({ "dataBase64": "aGk=" })).is_empty());
    assert!(shared().validate_request_by_id("terminals.resize", &json!({ "cols": 100, "rows": 30 })).is_empty());
    assert!(shared().validate_request_by_id("terminals.close", &json!({ "mode": "force", "reason": "user" })).is_empty());
    assert!(shared().validate_request_by_id("terminals.update", &json!({ "title": "build" })).is_empty());
    assert!(shared().validate_request_by_id("terminals.restart", &json!({})).is_empty());

    // The counter-examples: an extra field is not part of the contract, and a
    // `files.list` with no path cannot be answered.
    assert!(!shared().validate_request_by_id("files.list", &json!({ "path": "/srv", "bogus": true })).is_empty());
    assert!(!shared().validate_request_by_id("files.list", &json!({})).is_empty());
    // `files.write` requires content.
    assert!(!shared().validate_request_by_id("files.write", &json!({ "path": "a.txt" })).is_empty());
    // A terminal must have a target.
    assert!(!shared().validate_request_by_id("terminals.create", &json!({ "cols": 80, "rows": 24 })).is_empty());
}

/* ------------------------------------------------------------------ */
/* files.*                                                             */
/* ------------------------------------------------------------------ */

#[tokio::test]
async fn files_list_is_answered_from_the_host_not_the_server_disk() {
    let mut fixture = fixture().await;
    let host = fixture.host_id.to_string();
    fixture.answer_file(HostFileOutcome::Listing {
        entries: vec![
            file_entry("lib.rs", HostPathKind::File),
            file_entry("main.rs", HostPathKind::File),
        ],
        truncated: false,
    });

    let response = fixture
        .post(
            "/api/v1/files/list",
            Some(json!({ "hostId": host, "path": "/srv/b9/src" })),
        )
        .await;
    assert_eq!(response.status, 200, "{:?}", response.body);
    assert_response("files.list", response.status, &response.body);
    assert_eq!(response.body["truncated"], false);
    assert_eq!(response.body["files"].as_array().unwrap().len(), 2);
    // `fileSchema` is `{path, name}` only.
    assert_eq!(response.body["files"][0]["name"], "lib.rs");
    assert!(response.body["files"][0].get("kind").is_none());

    match fixture.next_file_request().await {
        HostFileOperation::ListDirectory {
            path,
            include_files,
            include_directories,
            ..
        } => {
            assert_eq!(path, "/srv/b9/src");
            assert!(include_files);
            assert!(!include_directories, "files.list is files only");
        }
        other => panic!("expected a directory listing, got {other:?}"),
    }
    fixture.state.shutdown();
}

#[tokio::test]
async fn files_list_with_a_query_becomes_a_recursive_search() {
    let mut fixture = fixture().await;
    let host = fixture.host_id.to_string();
    fixture.answer_file(HostFileOutcome::Listing {
        entries: vec![file_entry("src/main.rs", HostPathKind::File)],
        truncated: false,
    });
    let response = fixture
        .post(
            "/api/v1/files/list",
            Some(json!({ "hostId": host, "path": "/srv/b9", "query": "main" })),
        )
        .await;
    assert_eq!(response.status, 200);
    assert_response("files.list", response.status, &response.body);

    match fixture.next_file_request().await {
        HostFileOperation::List { query, .. } => assert_eq!(query.as_deref(), Some("main")),
        other => panic!("a query must become a recursive list, got {other:?}"),
    }
    fixture.state.shutdown();
}

#[tokio::test]
async fn files_list_paths_projects_kind_and_scores() {
    let fixture = fixture().await;
    let host = fixture.host_id.to_string();
    fixture.answer_file(HostFileOutcome::Listing {
        entries: vec![
            file_entry("src", HostPathKind::Directory),
            file_entry("src/main.rs", HostPathKind::File),
        ],
        truncated: true,
    });
    let response = fixture
        .post(
            "/api/v1/files/paths",
            Some(json!({
                "hostId": host,
                "path": "/srv/b9",
                "includeFiles": true,
                "includeDirectories": true,
            })),
        )
        .await;
    assert_eq!(response.status, 200, "{:?}", response.body);
    assert_response("files.listPaths", response.status, &response.body);
    assert_eq!(response.body["truncated"], true);
    assert_eq!(response.body["paths"][0]["kind"], "directory");
    assert_eq!(response.body["paths"][1]["kind"], "file");
    assert!(response.body["paths"][0]["positions"].is_array());
    fixture.state.shutdown();
}

#[tokio::test]
async fn files_read_returns_the_content_and_hash_without_touching_a_file() {
    let mut fixture = fixture().await;
    let host = fixture.host_id.to_string();
    fixture.answer_file(HostFileOutcome::FileMetadata {
        content: "fn main() {}\n".into(),
        content_encoding: HostFileEncoding::Utf8,
        size_bytes: 13,
        sha256: "abc123".into(),
        mode: Some(0o644),
        modified_at_ms: Some(5),
    });
    let response = fixture
        .post(
            "/api/v1/files/read",
            Some(json!({ "hostId": host, "path": "/srv/b9/main.rs" })),
        )
        .await;
    assert_eq!(response.status, 200, "{:?}", response.body);
    assert_response("files.read", response.status, &response.body);
    assert_eq!(response.body["content"], "fn main() {}\n");
    assert_eq!(response.body["contentEncoding"], "utf8");
    assert_eq!(response.body["sha256"], "abc123");
    assert_eq!(response.body["sizeBytes"], 13);

    match fixture.next_file_request().await {
        HostFileOperation::ReadWithMetadata {
            path, max_bytes, ..
        } => {
            assert_eq!(path, "/srv/b9/main.rs");
            assert!(max_bytes > 0);
        }
        other => panic!("expected a metadata read, got {other:?}"),
    }
    fixture.state.shutdown();
}

#[tokio::test]
async fn files_write_conflict_is_a_200_with_the_current_hash() {
    let mut fixture = fixture().await;
    let host = fixture.host_id.to_string();
    fixture.answer_file(HostFileOutcome::Conflict {
        current_sha256: Some("theirs".into()),
    });
    let response = fixture
        .post(
            "/api/v1/files/write",
            Some(json!({
                "hostId": host,
                "path": "notes.txt",
                "rootPath": WORKSPACE,
                "content": "mine",
                "expectedSha256": "theirs-expected",
            })),
        )
        .await;
    assert_eq!(response.status, 200, "{:?}", response.body);
    assert_response("files.write", response.status, &response.body);
    assert_eq!(response.body["outcome"], "conflict");
    assert_eq!(response.body["currentSha256"], "theirs");

    match fixture.next_file_request().await {
        HostFileOperation::WriteFile {
            expected_sha256,
            create_only,
            ..
        } => {
            assert_eq!(expected_sha256.as_deref(), Some("theirs-expected"));
            assert!(!create_only);
        }
        other => panic!("expected a write, got {other:?}"),
    }
    fixture.state.shutdown();
}

#[tokio::test]
async fn files_write_with_a_null_hash_is_create_only() {
    let mut fixture = fixture().await;
    let host = fixture.host_id.to_string();
    fixture.answer_file(HostFileOutcome::Written(HostFileContent {
        path: format!("{WORKSPACE}/new.txt"),
        content: String::new(),
        content_encoding: HostFileEncoding::Utf8,
        size_bytes: 3,
        mime_type: Some("text/plain".into()),
        modified_at_ms: None,
        sha256: Some("fresh".into()),
    }));
    let response = fixture
        .post(
            "/api/v1/files/write",
            Some(json!({
                "hostId": host,
                "path": "new.txt",
                "rootPath": WORKSPACE,
                "content": "abc",
                "expectedSha256": null,
            })),
        )
        .await;
    assert_eq!(response.status, 200, "{:?}", response.body);
    assert_response("files.write", response.status, &response.body);
    assert_eq!(response.body["outcome"], "written");
    assert_eq!(response.body["sha256"], "fresh");

    // `null` must reach the daemon as create-only, not as "no check".
    match fixture.next_file_request().await {
        HostFileOperation::WriteFile {
            create_only,
            expected_sha256,
            ..
        } => {
            assert!(create_only, "a null expectedSha256 means create-only");
            assert!(expected_sha256.is_none());
        }
        other => panic!("expected a write, got {other:?}"),
    }
    fixture.state.shutdown();
}

#[tokio::test]
async fn files_mkdir_move_and_remove_reach_the_host_with_the_root() {
    let mut fixture = fixture().await;
    let host = fixture.host_id.to_string();

    for _ in 0..3 {
        fixture.answer_file(HostFileOutcome::Done);
    }

    let mkdir = fixture
        .post(
            "/api/v1/files/mkdir",
            Some(json!({
                "hostId": host,
                "path": "src/new",
                "rootPath": WORKSPACE,
                "recursive": true,
            })),
        )
        .await;
    assert_eq!(mkdir.status, 200, "{:?}", mkdir.body);
    assert_response("files.mkdir", mkdir.status, &mkdir.body);
    assert_eq!(mkdir.body, json!({ "ok": true }));
    match fixture.next_file_request().await {
        HostFileOperation::CreateDirectory {
            path,
            root_path,
            recursive,
        } => {
            assert_eq!(path, format!("{WORKSPACE}/src/new"));
            assert_eq!(root_path.as_deref(), Some(WORKSPACE));
            assert!(recursive);
        }
        other => panic!("expected a mkdir, got {other:?}"),
    }

    let moved = fixture
        .post(
            "/api/v1/files/move",
            Some(json!({
                "hostId": host,
                "sourcePath": "a.txt",
                "destinationPath": "b.txt",
                "rootPath": WORKSPACE,
            })),
        )
        .await;
    assert_eq!(moved.status, 200, "{:?}", moved.body);
    assert_response("files.move", moved.status, &moved.body);
    match fixture.next_file_request().await {
        HostFileOperation::Move {
            source_path,
            destination_path,
            root_path,
            overwrite,
        } => {
            assert_eq!(source_path, format!("{WORKSPACE}/a.txt"));
            assert_eq!(destination_path, format!("{WORKSPACE}/b.txt"));
            assert_eq!(root_path.as_deref(), Some(WORKSPACE));
            assert!(!overwrite, "a move must not clobber by default");
        }
        other => panic!("expected a move, got {other:?}"),
    }

    let removed = fixture
        .post(
            "/api/v1/files/remove",
            Some(json!({ "hostId": host, "path": "old", "rootPath": WORKSPACE })),
        )
        .await;
    assert_eq!(removed.status, 200, "{:?}", removed.body);
    assert_response("files.remove", removed.status, &removed.body);
    match fixture.next_file_request().await {
        HostFileOperation::Remove {
            path,
            root_path,
            recursive,
        } => {
            assert_eq!(path, format!("{WORKSPACE}/old"));
            assert_eq!(root_path.as_deref(), Some(WORKSPACE));
            assert!(!recursive);
        }
        other => panic!("expected a remove, got {other:?}"),
    }
    fixture.state.shutdown();
}

#[tokio::test]
async fn a_traversing_file_path_is_refused_before_a_request_is_built() {
    let mut fixture = fixture().await;
    let host = fixture.host_id.to_string();
    for bad in ["../secret", "a/../../b", "/etc/passwd", "a\\..\\b", "a//b"] {
        let response = fixture
            .post(
                "/api/v1/files/read",
                Some(json!({ "hostId": host, "path": bad, "rootPath": WORKSPACE })),
            )
            .await;
        assert_status_and_error(&response, 400, "invalid_path");
    }
    // A relative path without a root is just as unresolvable.
    let bare = fixture
        .post(
            "/api/v1/files/read",
            Some(json!({ "hostId": host, "path": "rel.txt" })),
        )
        .await;
    assert_status_and_error(&bare, 400, "invalid_path");

    assert!(
        fixture.requests.try_recv().is_err(),
        "a refused path must never become a host request"
    );
    fixture.state.shutdown();
}

#[tokio::test]
async fn a_root_requiring_route_refuses_a_missing_root() {
    let mut fixture = fixture().await;
    let host = fixture.host_id.to_string();
    let response = fixture
        .post(
            "/api/v1/files/write",
            Some(json!({ "hostId": host, "path": "x.txt", "content": "hi" })),
        )
        .await;
    assert_status_and_error(&response, 400, "invalid_request");
    assert!(
        fixture.requests.try_recv().is_err(),
        "a write with no root must not reach a host"
    );
    fixture.state.shutdown();
}

#[tokio::test]
async fn an_unknown_host_is_a_404_not_a_local_fallback() {
    let fixture = fixture().await;
    let response = fixture
        .post(
            "/api/v1/files/list",
            Some(json!({ "hostId": UNKNOWN_HOST, "path": "/srv" })),
        )
        .await;
    assert_status_and_error(&response, 404, "host_not_found");
    fixture.state.shutdown();
}

#[tokio::test]
async fn a_host_file_failure_crosses_over_with_its_contract_code() {
    let fixture = fixture().await;
    let host = fixture.host_id.to_string();

    fixture.answer_file(HostFileOutcome::Failed {
        code: "not_found".into(),
        message: "no such file".into(),
    });
    let missing = fixture
        .post(
            "/api/v1/files/read",
            Some(json!({ "hostId": host, "path": "/srv/b9/gone.rs" })),
        )
        .await;
    assert_status_and_error(&missing, 404, "not_found");

    fixture.answer_file(HostFileOutcome::Failed {
        code: "file_too_large".into(),
        message: "enormous".into(),
    });
    let large = fixture
        .post(
            "/api/v1/files/read",
            Some(json!({ "hostId": host, "path": "/srv/b9/huge.bin" })),
        )
        .await;
    assert_status_and_error(&large, 413, "file_too_large");

    fixture.answer_file(HostFileOutcome::Failed {
        code: "conflict".into(),
        message: "destination exists".into(),
    });
    let conflict = fixture
        .post(
            "/api/v1/files/move",
            Some(json!({
                "hostId": host,
                "sourcePath": "a",
                "destinationPath": "b",
                "rootPath": WORKSPACE,
            })),
        )
        .await;
    assert_status_and_error(&conflict, 409, "conflict");
    fixture.state.shutdown();
}

/* ------------------------------------------------------------------ */
/* terminals.*                                                         */
/* ------------------------------------------------------------------ */

#[tokio::test]
async fn terminals_create_mints_an_id_and_asks_the_host() {
    let mut fixture = fixture().await;
    let host = fixture.host_id.to_string();
    fixture.answer_terminal(TerminalOutcome::Session {
        session: session("term_host_side", &fixture.host_id, TerminalStatus::Running),
    });

    let response = fixture
        .post(
            "/api/v1/terminals",
            Some(json!({
                "cols": 80,
                "rows": 24,
                "title": "build",
                "target": { "kind": "host_path", "hostId": host, "cwd": WORKSPACE },
            })),
        )
        .await;
    assert_eq!(response.status, 201, "{:?}", response.body);
    assert_response("terminals.create", response.status, &response.body);

    match fixture.next_terminal_request().await {
        TerminalOperation::Create {
            id,
            start,
            target,
            cols,
            rows,
            title,
            cwd,
        } => {
            assert!(id.starts_with("term_"), "the server mints the id: {id}");
            assert_eq!(start, loom_provider_protocol::TerminalStart::Shell);
            assert_eq!(cols, 80);
            assert_eq!(rows, 24);
            assert_eq!(title, "build");
            assert_eq!(cwd, WORKSPACE);
            match target {
                loom_provider_protocol::TerminalTarget::HostPath { cwd, .. } => {
                    assert_eq!(cwd.as_deref(), Some(WORKSPACE))
                }
                other => panic!("expected a host_path target, got {other:?}"),
            }
        }
        other => panic!("expected a create, got {other:?}"),
    }

    // The record is stored under the id the server minted, and carries the
    // ownership the plan resolved.
    let listed = fixture.get("/api/v1/terminals").await;
    assert_response("terminals.list", listed.status, &listed.body);
    let sessions = listed.body["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0]["hostId"], host);
    fixture.state.shutdown();
}

#[tokio::test]
async fn terminals_create_refuses_an_out_of_range_size_before_a_round_trip() {
    let mut fixture = fixture().await;
    let host = fixture.host_id.to_string();
    let response = fixture
        .post(
            "/api/v1/terminals",
            Some(json!({
                "cols": 0,
                "rows": 24,
                "target": { "kind": "host_path", "hostId": host, "cwd": WORKSPACE },
            })),
        )
        .await;
    // The contract's `exclusiveMinimum: 0` rejects it before the handler, and
    // either way nothing reaches the machine. The middleware's `422` precedes
    // any handler, so this checks the uniform error body rather than asserting
    // the code's declared status.
    assert_eq!(response.status, 422, "{:?}", response.body);
    assert_middleware_error(&response.body);
    assert!(fixture.requests.try_recv().is_err());
    fixture.state.shutdown();
}

#[tokio::test]
async fn an_unknown_terminal_is_404_on_every_route() {
    let fixture = fixture().await;
    let missing = "term_01M27Y6Q0J8V4W2C7K5N3P1R9Z";

    assert_status_and_error(
        &fixture.get(&format!("/api/v1/terminals/{missing}")).await,
        404,
        "terminal_not_found",
    );
    assert_status_and_error(
        &fixture
            .get(&format!("/api/v1/terminals/{missing}/output"))
            .await,
        404,
        "terminal_not_found",
    );
    assert_status_and_error(
        &fixture
            .post(
                &format!("/api/v1/terminals/{missing}/input"),
                Some(json!({ "dataBase64": "aGk=" })),
            )
            .await,
        404,
        "terminal_not_found",
    );
    assert_status_and_error(
        &fixture
            .post(
                &format!("/api/v1/terminals/{missing}/resize"),
                Some(json!({ "cols": 100, "rows": 30 })),
            )
            .await,
        404,
        "terminal_not_found",
    );
    assert_status_and_error(
        &fixture
            .post(
                &format!("/api/v1/terminals/{missing}/close"),
                Some(json!({ "mode": "force", "reason": "user" })),
            )
            .await,
        404,
        "terminal_not_found",
    );
    assert_status_and_error(
        &fixture
            .post(
                &format!("/api/v1/terminals/{missing}/restart"),
                Some(json!({})),
            )
            .await,
        404,
        "terminal_not_found",
    );
    assert_status_and_error(
        &fixture
            .patch(
                &format!("/api/v1/terminals/{missing}"),
                json!({ "title": "x" }),
            )
            .await,
        404,
        "terminal_not_found",
    );
    fixture.state.shutdown();
}

/// Creates one terminal through the HTTP surface and returns its id.
async fn create_terminal(fixture: &mut Fixture) -> String {
    let host = fixture.host_id.to_string();
    fixture.answer_terminal(TerminalOutcome::Session {
        session: session("ignored", &fixture.host_id, TerminalStatus::Running),
    });
    let response = fixture
        .post(
            "/api/v1/terminals",
            Some(json!({
                "cols": 80,
                "rows": 24,
                "target": { "kind": "host_path", "hostId": host, "cwd": WORKSPACE },
            })),
        )
        .await;
    assert_eq!(response.status, 201, "{:?}", response.body);
    fixture.next_terminal_request().await;
    response.body["id"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn terminals_input_and_resize_ask_the_host() {
    let mut fixture = fixture().await;
    let id = create_terminal(&mut fixture).await;

    fixture.answer_terminal(TerminalOutcome::Session {
        session: session(&id, &fixture.host_id, TerminalStatus::Running),
    });
    let input = fixture
        .post(
            &format!("/api/v1/terminals/{id}/input"),
            Some(json!({ "dataBase64": "bHMNCg==" })),
        )
        .await;
    assert_eq!(input.status, 200, "{:?}", input.body);
    assert_response("terminals.input", input.status, &input.body);
    match fixture.next_terminal_request().await {
        TerminalOperation::Input {
            id: asked,
            data_base64,
        } => {
            assert_eq!(asked, id);
            assert_eq!(data_base64, "bHMNCg==");
        }
        other => panic!("expected input, got {other:?}"),
    }

    fixture.answer_terminal(TerminalOutcome::Session {
        session: session(&id, &fixture.host_id, TerminalStatus::Running),
    });
    let resize = fixture
        .post(
            &format!("/api/v1/terminals/{id}/resize"),
            Some(json!({ "cols": 120, "rows": 40 })),
        )
        .await;
    assert_eq!(resize.status, 200, "{:?}", resize.body);
    assert_response("terminals.resize", resize.status, &resize.body);
    match fixture.next_terminal_request().await {
        TerminalOperation::Resize { cols, rows, .. } => {
            assert_eq!((cols, rows), (120, 40));
        }
        other => panic!("expected a resize, got {other:?}"),
    }
    fixture.state.shutdown();
}

#[tokio::test]
async fn terminals_input_refuses_empty_data_without_a_round_trip() {
    let mut fixture = fixture().await;
    let id = create_terminal(&mut fixture).await;
    let response = fixture
        .post(
            &format!("/api/v1/terminals/{id}/input"),
            Some(json!({ "dataBase64": "" })),
        )
        .await;
    // The contract's `minLength: 1` rejects it before the handler.
    assert_eq!(response.status, 422, "{:?}", response.body);
    assert!(
        fixture.requests.try_recv().is_err(),
        "empty input must not reach the host"
    );
    fixture.state.shutdown();
}

#[tokio::test]
async fn terminals_output_reads_a_window_and_preserves_the_cursor() {
    let mut fixture = fixture().await;
    let id = create_terminal(&mut fixture).await;

    fixture.answer_terminal(TerminalOutcome::Output {
        chunks: vec![
            loom_provider_protocol::TerminalOutputChunk {
                seq: 3,
                data_base64: "aGVsbG8=".into(),
            },
            loom_provider_protocol::TerminalOutputChunk {
                seq: 4,
                data_base64: "IHdvcmxk".into(),
            },
        ],
        next_seq: 5,
        truncated: true,
    });
    let response = fixture
        .get(&format!(
            "/api/v1/terminals/{id}/output?sinceSeq=3&limitChunks=10"
        ))
        .await;
    assert_eq!(response.status, 200, "{:?}", response.body);
    assert_response("terminals.output", response.status, &response.body);
    assert_eq!(response.body["nextSeq"], 5);
    assert_eq!(response.body["truncated"], true);
    assert_eq!(response.body["chunks"].as_array().unwrap().len(), 2);

    match fixture.next_terminal_request().await {
        TerminalOperation::Output {
            id: asked,
            since_seq,
            limit,
            tail_bytes,
        } => {
            assert_eq!(asked, id);
            assert_eq!(since_seq, 3);
            assert_eq!(limit, 10);
            assert!(tail_bytes > 0);
        }
        other => panic!("expected an output read, got {other:?}"),
    }
    fixture.state.shutdown();
}

#[tokio::test]
async fn terminals_close_reports_a_not_running_failure_at_409() {
    let mut fixture = fixture().await;
    let id = create_terminal(&mut fixture).await;
    fixture.answer_terminal(TerminalOutcome::Failed {
        code: "terminal_not_running".into(),
        message: "the terminal is still running; close it with force".into(),
    });
    let response = fixture
        .post(
            &format!("/api/v1/terminals/{id}/close"),
            Some(json!({ "mode": "if-clean", "reason": "user" })),
        )
        .await;
    assert_status_and_error(&response, 409, "terminal_not_running");
    match fixture.next_terminal_request().await {
        TerminalOperation::Close { force, .. } => assert!(!force),
        other => panic!("expected a close, got {other:?}"),
    }
    fixture.state.shutdown();
}

#[tokio::test]
async fn terminals_close_refuses_an_unknown_mode_before_a_round_trip() {
    let mut fixture = fixture().await;
    let id = create_terminal(&mut fixture).await;
    // The contract's `enum` rejects it, so this is the middleware's 422.
    let response = fixture
        .post(
            &format!("/api/v1/terminals/{id}/close"),
            Some(json!({ "mode": "whenever", "reason": "user" })),
        )
        .await;
    assert_eq!(response.status, 422, "{:?}", response.body);
    assert!(fixture.requests.try_recv().is_err());
    fixture.state.shutdown();
}

#[tokio::test]
async fn terminals_restart_answers_201_with_the_fresh_session() {
    let mut fixture = fixture().await;
    let id = create_terminal(&mut fixture).await;
    fixture.answer_terminal(TerminalOutcome::Session {
        session: session(&id, &fixture.host_id, TerminalStatus::Running),
    });
    let response = fixture
        .post(&format!("/api/v1/terminals/{id}/restart"), Some(json!({})))
        .await;
    assert_eq!(response.status, 201, "{:?}", response.body);
    assert_response("terminals.restart", response.status, &response.body);
    match fixture.next_terminal_request().await {
        TerminalOperation::Restart { id: asked } => assert_eq!(asked, id),
        other => panic!("expected a restart, got {other:?}"),
    }
    fixture.state.shutdown();
}

#[tokio::test]
async fn terminals_update_renames_without_asking_the_host() {
    let mut fixture = fixture().await;
    let id = create_terminal(&mut fixture).await;
    let response = fixture
        .patch(
            &format!("/api/v1/terminals/{id}"),
            json!({ "title": "build watch" }),
        )
        .await;
    assert_eq!(response.status, 200, "{:?}", response.body);
    assert_response("terminals.update", response.status, &response.body);
    assert_eq!(response.body["title"], "build watch");
    // A rename is control-plane metadata: nothing reaches the machine.
    assert!(fixture.requests.try_recv().is_err());

    let read = fixture.get(&format!("/api/v1/terminals/{id}")).await;
    assert_eq!(read.body["title"], "build watch");
    fixture.state.shutdown();
}

#[tokio::test]
async fn terminals_list_filters_by_ownership() {
    let mut fixture = fixture().await;
    let id = create_terminal(&mut fixture).await;

    let by_host = fixture
        .get(&format!("/api/v1/terminals?hostId={}", fixture.host_id))
        .await;
    assert_eq!(by_host.body["sessions"].as_array().unwrap().len(), 1);

    let other = fixture
        .get(&format!("/api/v1/terminals?hostId={UNKNOWN_HOST}"))
        .await;
    assert_eq!(other.body["sessions"].as_array().unwrap().len(), 0);

    let by_thread = fixture
        .get("/api/v1/terminals?threadId=thr_01M27Y6Q0J8V4W2C7K5N3P1R9Z")
        .await;
    assert_eq!(by_thread.body["sessions"].as_array().unwrap().len(), 0);
    assert_eq!(by_host.body["sessions"][0]["id"], id);
    fixture.state.shutdown();
}

#[tokio::test]
async fn a_thread_targeted_terminal_uses_the_environments_host() {
    let mut fixture = fixture().await;
    let now = loom_relay::now_ms();
    let (project, _) = fixture
        .state
        .registry
        .create_project("b9".into(), ProjectKind::Standard, None, now)
        .unwrap();
    let (environment, _) = fixture
        .state
        .registry
        .create_environment(
            Some(project.id.clone()),
            fixture.host_id.clone(),
            EnvironmentKind::Unmanaged,
            Some("/srv/env".into()),
            now,
        )
        .unwrap();
    let (thread, _) = fixture
        .state
        .registry
        .create_thread(
            Some(project.id.clone()),
            Some("t".into()),
            Some(environment.id.clone()),
            now,
        )
        .unwrap();

    fixture.answer_terminal(TerminalOutcome::Session {
        session: TerminalSession {
            thread_id: Some(thread.id.clone()),
            environment_id: Some(environment.id.clone()),
            initial_cwd: "/srv/env".into(),
            ..session("ignored", &fixture.host_id, TerminalStatus::Running)
        },
    });
    let response = fixture
        .post(
            "/api/v1/terminals",
            Some(json!({
                "cols": 80,
                "rows": 24,
                "target": { "kind": "thread", "threadId": thread.id.to_string() },
            })),
        )
        .await;
    assert_eq!(response.status, 201, "{:?}", response.body);
    assert_response("terminals.create", response.status, &response.body);
    assert_eq!(response.body["threadId"], thread.id.to_string());
    assert_eq!(response.body["environmentId"], environment.id.to_string());
    assert_eq!(response.body["hostId"], fixture.host_id.to_string());

    match fixture.next_terminal_request().await {
        TerminalOperation::Create { cwd, target, .. } => {
            assert_eq!(cwd, "/srv/env");
            assert!(matches!(
                target,
                loom_provider_protocol::TerminalTarget::Thread { .. }
            ));
        }
        other => panic!("expected a create, got {other:?}"),
    }
    fixture.state.shutdown();
}

#[tokio::test]
async fn a_thread_without_an_environment_is_refused() {
    let mut fixture = fixture().await;
    let now = loom_relay::now_ms();
    let (project, _) = fixture
        .state
        .registry
        .create_project("b9-bare".into(), ProjectKind::Standard, None, now)
        .unwrap();
    let (thread, _) = fixture
        .state
        .registry
        .create_thread(Some(project.id.clone()), Some("t".into()), None, now)
        .unwrap();
    let response = fixture
        .post(
            "/api/v1/terminals",
            Some(json!({
                "cols": 80,
                "rows": 24,
                "target": { "kind": "thread", "threadId": thread.id.to_string() },
            })),
        )
        .await;
    assert_status_and_error(&response, 409, "thread_environment_unavailable");
    assert!(fixture.requests.try_recv().is_err());
    fixture.state.shutdown();
}

#[tokio::test]
async fn deleting_a_thread_settles_its_terminals() {
    let mut fixture = fixture().await;
    let now = loom_relay::now_ms();
    let (project, _) = fixture
        .state
        .registry
        .create_project("b9-del".into(), ProjectKind::Standard, None, now)
        .unwrap();
    let (environment, _) = fixture
        .state
        .registry
        .create_environment(
            Some(project.id.clone()),
            fixture.host_id.clone(),
            EnvironmentKind::Unmanaged,
            Some("/srv/env".into()),
            now,
        )
        .unwrap();
    let (thread, _) = fixture
        .state
        .registry
        .create_thread(
            Some(project.id.clone()),
            Some("t".into()),
            Some(environment.id.clone()),
            now,
        )
        .unwrap();

    let terminal_id = "term_delete_me";
    let mut stored = session(terminal_id, &fixture.host_id, TerminalStatus::Running);
    stored.thread_id = Some(thread.id.clone());
    stored.environment_id = Some(environment.id.clone());
    fixture.state.terminals.put(stored);

    let deleted = fixture
        .delete(
            &format!("/api/v1/threads/{}", thread.id),
            Some(json!({ "childThreadsConfirmed": true })),
        )
        .await;
    assert_eq!(deleted.status, 200, "{:?}", deleted.body);

    // The record is settled immediately; the daemon is asked to kill the
    // process in the background.
    let read = fixture
        .get(&format!("/api/v1/terminals/{terminal_id}"))
        .await;
    assert_eq!(read.body["status"], "exited");
    assert_eq!(read.body["closeReason"], "thread-deleted");

    // The close request reaches the host.
    match fixture.next_terminal_request().await {
        TerminalOperation::Close { id, force } => {
            assert_eq!(id, terminal_id);
            assert!(force, "a deleted thread's process must be killed");
        }
        other => panic!("expected a close, got {other:?}"),
    }
    fixture.state.shutdown();
}

#[tokio::test]
async fn a_disconnected_host_marks_its_terminals_undrivable() {
    let mut fixture = fixture().await;
    let id = create_terminal(&mut fixture).await;
    assert_eq!(
        fixture.get(&format!("/api/v1/terminals/{id}")).await.body["status"],
        "running"
    );

    fixture
        .state
        .terminals
        .mark_host_disconnected(&fixture.host_id, loom_relay::now_ms());
    let read = fixture.get(&format!("/api/v1/terminals/{id}")).await;
    assert_eq!(read.body["status"], "disconnected");
    // The process may still be alive on that machine; only the status changed.
    assert!(read.body["exitCode"].is_null());
    fixture.state.shutdown();
}

#[tokio::test]
async fn a_terminal_failure_is_reported_and_does_not_fabricate_output() {
    let mut fixture = fixture().await;
    let id = create_terminal(&mut fixture).await;
    fixture.answer_terminal(TerminalOutcome::Failed {
        code: "terminal_output_unavailable".into(),
        message: "the host lost the output ring".into(),
    });
    let response = fixture.get(&format!("/api/v1/terminals/{id}/output")).await;
    // An unknown daemon code becomes the generic host failure at the same
    // status rather than inventing an error code no client can branch on.
    assert_status_and_error(&response, 502, "host_unavailable");
    fixture.state.shutdown();
}
