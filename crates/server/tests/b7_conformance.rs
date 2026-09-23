//! B7 conformance: project workspace, attachments and thread sections.
//!
//! The file and attachment routes are exercised with a **scripted host**: a real
//! WebSocket connection that enrolls as a host, receives whatever the control
//! plane published to its room, and answers with the outcome a real worker would
//! send. That is the property this batch must prove — a project file read never
//! touches the server's own disk, and an upload's path is confined to the
//! project's attachment root on the machine that owns it — and a scripted host
//! can assert exactly which operation it was asked to perform.
//!
//! `projects.commands` is a host RPC rather than a file operation, so the same
//! scripted host answers `host_rpc_report` frames too.
//!
//! The filesystem semantics themselves (containment, base64, symlink escapes,
//! the suffix a colliding copy gets) are covered against the real
//! implementation in `crates/worker`, because that is where the filesystem is.
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
    project_attachments_root, HostFileContent, HostFileEncoding, HostFileEntry, HostFileFailure,
    HostFileOperation, HostFileOutcome, HostFileReport, HostPathKind, HostRpcOperation,
    HostRpcOutcome, HostRpcReport, ProviderCommand, ProviderLaunch, ProviderSpec,
};
use loom_server::http::router;
use loom_server::state::{AppConfig, AppState};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

const TIMEOUT: Duration = Duration::from_secs(10);
const UNKNOWN_PROJECT: &str = "proj_01M27Y6Q0J8V4W2C7K5N3P1R9Z";
const UNKNOWN_SECTION: &str = "sec_01M27Y6Q0J8V4W2C7K5N3P1R9Z";
const DATA_DIR: &str = "/var/lib/loom";
const WORKSPACE: &str = "/srv/b7";

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
    headers: Vec<(String, String)>,
    bytes: Vec<u8>,
}

impl Response {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
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
    let headers = head
        .lines()
        .skip(1)
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
        .collect();
    let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    Response {
        status,
        body,
        headers,
        bytes,
    }
}

/// Sends a raw multipart/form-data body, which is what `projects.uploadAttachment`
/// declares (`source: "form"`) and what the JSON helper cannot express.
async fn upload(addr: &str, path: &str, field: &str, file_name: &str, bytes: &[u8]) -> Response {
    let boundary = "----loomb7boundary";
    let mut body = Vec::new();
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(
        format!(
            "Content-Disposition: form-data; name=\"{field}\"; filename=\"{file_name}\"\r\n\
             Content-Type: application/octet-stream\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(bytes);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());

    let mut stream = TcpStream::connect(addr).await.unwrap();
    let head = format!(
        "POST {path} HTTP/1.1\r\nHost: {addr}\r\n\
         Content-Type: multipart/form-data; boundary={boundary}\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await.unwrap();
    stream.write_all(&body).await.unwrap();
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
    Response {
        status,
        body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        headers: Vec::new(),
        bytes,
    }
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

/* ------------------------------------------------------------------ */
/* Fixture                                                             */
/* ------------------------------------------------------------------ */

enum HostRequest {
    File(HostFileOperation),
    Rpc(HostRpcOperation),
}

struct Fixture {
    addr: String,
    state: AppState,
    project_id: String,
    personal_project_id: String,
    host_id: HostId,
    attachments_root: String,
    /// Requests the scripted host received, in order.
    requests: mpsc::UnboundedReceiver<HostRequest>,
    /// Scripted file answers, consumed in order. An empty queue replays the last
    /// outcome, so the common case needs one.
    file_script: mpsc::UnboundedSender<HostFileOutcome>,
    /// Scripted host RPC answers, consumed in order.
    rpc_script: mpsc::UnboundedSender<HostRpcOutcome>,
    /// The provider name this server runs, for `projects.commands`.
    provider: String,
    host: tokio::task::JoinHandle<()>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.state.shutdown().unwrap();
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
            HostRequest::Rpc(operation) => {
                panic!("expected a file request, got a host rpc: {operation:?}")
            }
        }
    }

    async fn next_rpc_request(&mut self) -> HostRpcOperation {
        match self.next_request().await {
            HostRequest::Rpc(operation) => operation,
            HostRequest::File(operation) => {
                panic!("expected a host rpc, got a file request: {operation:?}")
            }
        }
    }

    fn answer_file(&self, outcome: HostFileOutcome) {
        self.file_script.send(outcome).unwrap();
    }

    fn answer_rpc(&self, outcome: HostRpcOutcome) {
        self.rpc_script.send(outcome).unwrap();
    }

    fn get<'a>(&'a self, path: &'a str) -> impl Future<Output = Response> + 'a {
        request(&self.addr, "GET", path, None)
    }

    fn post(&self, path: &str, body: Option<Value>) -> impl Future<Output = Response> + '_ {
        let addr = self.addr.clone();
        let path = path.to_owned();
        async move { request(&addr, "POST", &path, body).await }
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
    let (rpc_script_tx, rpc_script_rx) = mpsc::unbounded_channel::<HostRpcOutcome>();
    let host_id = spawn_scripted_host(&addr, requests_tx, file_script_rx, rpc_script_rx).await;

    let now = loom_relay::now_ms();
    let (project, _) = state
        .registry
        .create_project("b7".into(), ProjectKind::Standard, None, now)
        .unwrap();
    state
        .registry
        .add_project_source(&project.id, host_id.clone(), WORKSPACE.into(), None, now)
        .unwrap();
    let attachments_root = project_attachments_root(DATA_DIR, &project.id.to_string());

    let personal_project_id = state.registry.personal_project_id().to_string();
    let provider = state.provider_spec().name.clone();
    Fixture {
        addr,
        state,
        project_id: project.id.to_string(),
        personal_project_id,
        host_id,
        attachments_root,
        requests,
        file_script: file_script_tx,
        rpc_script: rpc_script_tx,
        provider,
        host: tokio::spawn(async {}),
    }
}

/// Enrolls a scripted host over a real socket and answers what it is asked.
///
/// A stand-in for a worker and nothing more. It answers both the file protocol
/// and the host RPC protocol, because `projects.commands` uses the second.
async fn spawn_scripted_host(
    addr: &str,
    requests: mpsc::UnboundedSender<HostRequest>,
    mut file_script: mpsc::UnboundedReceiver<HostFileOutcome>,
    mut rpc_script: mpsc::UnboundedReceiver<HostRpcOutcome>,
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
                "name": "b7-scripted",
                "data_dir": DATA_DIR,
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
                    Err(_) => match last_file.clone() {
                        Some(outcome) => outcome,
                        None => HostFileOutcome::Failed {
                            code: "not_found".into(),
                            message: "the scripted host has no answer".into(),
                        },
                    },
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
                serde_json::from_value::<loom_provider_protocol::HostRpcRequest>(payload)
            {
                let _ = requests.send(HostRequest::Rpc(request.operation.clone()));
                let outcome = rpc_script.try_recv().unwrap_or(HostRpcOutcome::Failed {
                    code: "unknown".into(),
                    message: "the scripted host has no rpc answer".into(),
                });
                let report = HostRpcReport {
                    host_id: task_host_id.clone(),
                    request_id: request.request_id,
                    outcome,
                };
                let _ = socket
                    .send(tokio_tungstenite::tungstenite::Message::Text(
                        json!({ "type": "host_rpc_report", "report": report })
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
    // `name` is the final path segment, exactly as the worker reports it.
    let name = path.rsplit('/').next().unwrap_or(path).to_owned();
    HostFileEntry {
        path: path.to_owned(),
        name,
        kind,
        score: 0.0,
        positions: Vec::new(),
    }
}

fn content(path: &str, text: &str) -> HostFileOutcome {
    HostFileOutcome::Content(HostFileContent {
        path: path.to_owned(),
        content: text.to_owned(),
        content_encoding: HostFileEncoding::Utf8,
        size_bytes: text.len() as u64,
        mime_type: Some("text/plain".into()),
        modified_at_ms: None,
        sha256: None,
    })
}

/* ------------------------------------------------------------------ */
/* Request-shape tests (the counter-examples the criteria require)      */
/* ------------------------------------------------------------------ */

#[test]
fn b7_json_write_requests_are_contract_shaped() {
    let contract = shared();

    // `projects.updateSource`: `type` is required and const.
    let valid = json!({ "type": "local_path" });
    assert!(
        contract
            .validate_request_by_id("projects.updateSource", &valid)
            .is_empty(),
        "projects.updateSource rejects a contract-shaped body"
    );
    for invalid in [
        json!({}),
        json!({ "type": "remote" }),
        json!({ "type": "local_path", "path": "" }),
        json!({ "type": "local_path", "isDefault": false }),
        json!({ "type": "local_path", "extra": 1 }),
    ] {
        assert!(
            !contract
                .validate_request_by_id("projects.updateSource", &invalid)
                .is_empty(),
            "projects.updateSource accepts an out-of-contract body: {invalid}"
        );
    }

    // `projects.reorder`: both neighbours are required, and each is either a
    // non-empty string or null.
    let valid = json!({ "previousProjectId": null, "nextProjectId": null });
    assert!(
        contract
            .validate_request_by_id("projects.reorder", &valid)
            .is_empty(),
        "projects.reorder rejects a contract-shaped body"
    );
    for invalid in [
        json!({}),
        json!({ "previousProjectId": null }),
        json!({ "previousProjectId": "", "nextProjectId": null }),
    ] {
        assert!(
            !contract
                .validate_request_by_id("projects.reorder", &invalid)
                .is_empty(),
            "projects.reorder accepts an out-of-contract body: {invalid}"
        );
    }

    // `projects.copyAttachments`: both fields required, and the path list is
    // bounded to at most 100 entries.
    let valid = json!({ "sourceProjectId": "proj_x", "paths": ["/srv/a"] });
    assert!(
        contract
            .validate_request_by_id("projects.copyAttachments", &valid)
            .is_empty(),
        "projects.copyAttachments rejects a contract-shaped body"
    );
    for invalid in [
        json!({}),
        json!({ "sourceProjectId": "proj_x" }),
        json!({ "sourceProjectId": "proj_x", "paths": [] }),
        json!({ "sourceProjectId": "proj_x", "paths": [""] }),
    ] {
        assert!(
            !contract
                .validate_request_by_id("projects.copyAttachments", &invalid)
                .is_empty(),
            "projects.copyAttachments accepts an out-of-contract body: {invalid}"
        );
    }
    let too_many = (0..101)
        .map(|index| Value::String(format!("/srv/a{index}")))
        .collect::<Vec<_>>();
    assert!(
        !contract
            .validate_request_by_id(
                "projects.copyAttachments",
                &json!({ "sourceProjectId": "proj_x", "paths": too_many }),
            )
            .is_empty(),
        "projects.copyAttachments accepts more than 100 paths"
    );

    // `threadSections.create`: a non-empty name only.
    assert!(contract
        .validate_request_by_id("threadSections.create", &json!({ "name": "Backlog" }))
        .is_empty());
    for invalid in [
        json!({}),
        json!({ "name": "" }),
        json!({ "name": "a", "id": "x" }),
    ] {
        assert!(
            !contract
                .validate_request_by_id("threadSections.create", &invalid)
                .is_empty(),
            "threadSections.create accepts {invalid}"
        );
    }

    // `threadSections.update` and `.delete` both require `id`.
    assert!(contract
        .validate_request_by_id(
            "threadSections.update",
            &json!({ "id": "sec_x", "name": "Later" }),
        )
        .is_empty());
    assert!(contract
        .validate_request_by_id("threadSections.delete", &json!({ "id": "sec_x" }))
        .is_empty());
    for invalid in [json!({}), json!({ "id": "" }), json!({ "name": "Later" })] {
        assert!(
            !contract
                .validate_request_by_id("threadSections.delete", &invalid)
                .is_empty(),
            "threadSections.delete accepts {invalid}"
        );
        assert!(
            !contract
                .validate_request_by_id("threadSections.update", &invalid)
                .is_empty(),
            "threadSections.update accepts {invalid}"
        );
    }
}

/* ------------------------------------------------------------------ */
/* Project workspace file routes                                        */
/* ------------------------------------------------------------------ */

#[tokio::test]
async fn project_files_lists_through_the_owning_host() {
    let mut fixture = fixture().await;
    fixture.answer_file(HostFileOutcome::Listing {
        entries: vec![
            file_entry("src/main.rs", HostPathKind::File),
            file_entry("README.md", HostPathKind::File),
        ],
        truncated: false,
    });

    let response = fixture
        .get(&format!("/api/v1/projects/{}/files", fixture.project_id))
        .await;
    assert_eq!(response.status, 200);
    assert_response("projects.files", response.status, &response.body);
    assert_eq!(response.body["truncated"], false);
    assert_eq!(response.body["files"][0]["path"], "src/main.rs");
    assert_eq!(response.body["files"][0]["name"], "main.rs");

    match fixture.next_file_request().await {
        HostFileOperation::List {
            path,
            include_files,
            include_directories,
            ..
        } => {
            assert_eq!(path, WORKSPACE);
            assert!(include_files);
            // `projects.files` is a file picker; directories are `projects.paths`.
            assert!(!include_directories);
        }
        other => panic!("expected a list on the default source, got {other:?}"),
    }
}

#[tokio::test]
async fn project_paths_honours_the_requested_kinds() {
    let mut fixture = fixture().await;
    fixture.answer_file(HostFileOutcome::Listing {
        entries: vec![
            file_entry("src", HostPathKind::Directory),
            file_entry("src/main.rs", HostPathKind::File),
        ],
        truncated: true,
    });

    let response = fixture
        .get(&format!(
            "/api/v1/projects/{}/paths?includeFiles=true&includeDirectories=true",
            fixture.project_id
        ))
        .await;
    assert_eq!(response.status, 200);
    assert_response("projects.paths", response.status, &response.body);
    assert_eq!(response.body["truncated"], true);
    assert_eq!(response.body["paths"][0]["kind"], "directory");

    match fixture.next_file_request().await {
        HostFileOperation::List {
            include_files,
            include_directories,
            ..
        } => {
            assert!(include_files);
            assert!(include_directories);
        }
        other => panic!("expected a list, got {other:?}"),
    }

    // Neither kind is a `400`, not an empty answer.
    let neither = fixture
        .get(&format!(
            "/api/v1/projects/{}/paths?includeFiles=false&includeDirectories=false",
            fixture.project_id
        ))
        .await;
    assert_status_and_error(&neither, 400, "invalid_request");

    // A non-boolean is rejected too, rather than coerced.
    let bogus = fixture
        .get(&format!(
            "/api/v1/projects/{}/paths?includeFiles=yes&includeDirectories=true",
            fixture.project_id
        ))
        .await;
    assert_status_and_error(&bogus, 400, "invalid_request");
}

#[tokio::test]
async fn project_file_content_reads_inside_the_workspace_root() {
    let mut fixture = fixture().await;
    fixture.answer_file(content("/srv/b7/src/main.rs", "fn main() {}"));

    let response = fixture
        .get(&format!(
            "/api/v1/projects/{}/files/content?path=src/main.rs",
            fixture.project_id
        ))
        .await;
    assert_eq!(response.status, 200);
    assert_eq!(response.bytes, b"fn main() {}");
    assert_eq!(response.header("content-type"), Some("text/plain"));
    assert_eq!(response.header("x-content-type-options"), Some("nosniff"));

    match fixture.next_file_request().await {
        HostFileOperation::Read {
            path,
            root_path,
            max_bytes,
        } => {
            assert_eq!(path, "/srv/b7/src/main.rs");
            // The root travels with the request, so the worker can refuse a
            // symlink that leaves the workspace.
            assert_eq!(root_path.as_deref(), Some(WORKSPACE));
            assert!(max_bytes > 0);
        }
        other => panic!("expected a read, got {other:?}"),
    }
}

#[tokio::test]
async fn project_file_paths_are_refused_before_a_request_is_built() {
    let mut fixture = fixture().await;
    for traversal in [
        "../secret",
        "src/../../etc/passwd",
        "/etc/passwd",
        "a//b",
        "a\\..\\b",
    ] {
        let response = fixture
            .get(&format!(
                "/api/v1/projects/{}/files/content?path={}",
                fixture.project_id,
                urlencode(traversal)
            ))
            .await;
        assert_status_and_error(&response, 400, "invalid_path");
    }
    // Nothing reached the host: a bad path never becomes a request.
    assert!(
        fixture.requests.try_recv().is_err(),
        "a refused path must not be forwarded to the host"
    );
}

#[tokio::test]
async fn project_file_routes_need_a_project_and_a_source() {
    let fixture = fixture().await;

    let unknown = fixture
        .get(&format!("/api/v1/projects/{UNKNOWN_PROJECT}/files"))
        .await;
    assert_status_and_error(&unknown, 404, "project_not_found");

    // A project with no source cannot answer a file question at all.
    let bare = fixture
        .state
        .registry
        .create_project(
            "bare".into(),
            ProjectKind::Standard,
            None,
            loom_relay::now_ms(),
        )
        .unwrap()
        .0;
    let no_source = fixture
        .get(&format!("/api/v1/projects/{}/files", bare.id))
        .await;
    assert_status_and_error(&no_source, 404, "not_found");

    // A source that declares a repository but has no checkout yet is not an
    // empty directory.
    let (remote_only, _) = fixture
        .state
        .registry
        .create_project(
            "remote".into(),
            ProjectKind::Standard,
            None,
            loom_relay::now_ms(),
        )
        .unwrap();
    fixture
        .state
        .registry
        .add_project_source(
            &remote_only.id,
            fixture.host_id.clone(),
            String::new(),
            Some("git@example.com:o/r.git".into()),
            loom_relay::now_ms(),
        )
        .unwrap();
    let unready = fixture
        .get(&format!("/api/v1/projects/{}/files", remote_only.id))
        .await;
    assert_status_and_error(&unready, 409, "conflict");
}

#[tokio::test]
async fn a_host_failure_crosses_over_with_its_contract_code() {
    let fixture = fixture().await;
    fixture.answer_file(HostFileOutcome::Failed {
        code: "file_too_large".into(),
        message: "the file is enormous".into(),
    });
    let too_large = fixture
        .get(&format!(
            "/api/v1/projects/{}/files/content?path=huge.bin",
            fixture.project_id
        ))
        .await;
    assert_status_and_error(&too_large, 413, "file_too_large");

    fixture.answer_file(HostFileOutcome::Failed {
        code: "not_found".into(),
        message: "no such file".into(),
    });
    let missing = fixture
        .get(&format!(
            "/api/v1/projects/{}/files/content?path=missing.txt",
            fixture.project_id
        ))
        .await;
    assert_status_and_error(&missing, 404, "not_found");
}

/* ------------------------------------------------------------------ */
/* Commands                                                            */
/* ------------------------------------------------------------------ */

#[tokio::test]
async fn project_commands_asks_the_host_and_projects_its_rows() {
    let mut fixture = fixture().await;
    fixture.answer_rpc(HostRpcOutcome::Result {
        result: json!({
            "commands": [
                { "name": "compact", "origin": "builtin", "description": "Compact", "argumentHint": null },
                { "name": "review", "origin": "project", "description": "Review the diff", "argumentHint": "[range]" },
                { "name": "handoff", "origin": "user", "description": "Hand off the session", "argumentHint": null },
            ]
        }),
    });

    let provider = fixture.provider.clone();
    let response = fixture
        .get(&format!(
            "/api/v1/projects/{}/commands?provider={provider}",
            fixture.project_id
        ))
        .await;
    assert_eq!(response.status, 200);
    assert_response("projects.commands", response.status, &response.body);
    assert_eq!(response.body["commands"][0]["origin"], "builtin");
    assert_eq!(response.body["commands"][0]["source"], "command");
    assert_eq!(response.body["commands"][1]["origin"], "project");
    assert_eq!(response.body["commands"][1]["argumentHint"], "[range]");
    // The contract has three origins and the worker now produces all of them;
    // the projection must not collapse a user prompt into a project one.
    assert_eq!(response.body["commands"][2]["origin"], "user");

    match fixture.next_rpc_request().await {
        HostRpcOperation::ListCommands { cwd } => assert_eq!(cwd, WORKSPACE),
        other => panic!("expected a command listing, got {other:?}"),
    }

    // `provider` is required, and a provider nothing offers is a `400` rather
    // than a silently different answer.
    let missing = fixture
        .get(&format!("/api/v1/projects/{}/commands", fixture.project_id))
        .await;
    assert_eq!(missing.status, 400);
    let other = fixture
        .get(&format!(
            "/api/v1/projects/{}/commands?provider=someone-else",
            fixture.project_id
        ))
        .await;
    assert_status_and_error(&other, 400, "invalid_request");
}

/// A discovered ACP agent over its own transport.
fn discovered_provider(name: &str, command: &str) -> ProviderSpec {
    ProviderSpec {
        name: name.into(),
        launch: ProviderLaunch::AcpStdio,
        command: command.into(),
        args: vec!["acp".into()],
        cwd: None,
    }
}

/// A discovered agent answers with its own ACP advertisement, never pi's scan.
///
/// `provider` names the agent the client will run, so a host that discovered
/// OMP must be able to ask for OMP's command list. Only the embedded pi-acp
/// adapter has a scan loom can run on disk, so an OMP query must not ask the
/// host at all — pi's built-ins are not commands OMP accepts. The rows are
/// attributed by name, exactly as an advertisement merged over a scan is.
#[tokio::test]
async fn project_commands_answers_a_discovered_provider_from_its_advertisement() {
    let mut fixture = fixture().await;
    fixture.state.record_host_providers(
        &fixture.host_id,
        vec![
            ProviderSpec::pi(),
            discovered_provider("omp", "/usr/bin/omp"),
        ],
    );
    // A stand-in for pi's scan: if the route asked the host, this is what an
    // OMP answer would wrongly include.
    fixture.answer_rpc(HostRpcOutcome::Result {
        result: json!({
            "commands": [
                { "name": "autocompact", "origin": "builtin", "description": "pi's builtin", "argumentHint": null },
            ]
        }),
    });
    // What `omp acp` advertised for a session in this workspace.
    fixture.state.commands.record(
        &fixture.host_id,
        "omp",
        WORKSPACE,
        vec![
            ProviderCommand {
                name: "review".into(),
                description: "Launch interactive code review".into(),
                argument_hint: Some("arguments".into()),
            },
            ProviderCommand {
                name: "skill:omp-only".into(),
                description: "A skill OMP advertises".into(),
                argument_hint: None,
            },
        ],
    );

    let (environment, _) = fixture
        .state
        .registry
        .create_environment(
            Some(fixture.project_id.parse().unwrap()),
            fixture.host_id.clone(),
            EnvironmentKind::Unmanaged,
            Some(WORKSPACE.into()),
            loom_relay::now_ms(),
        )
        .unwrap();
    let response = fixture
        .get(&format!(
            "/api/v1/projects/{}/commands?provider=omp&environmentId={}",
            fixture.project_id, environment.id
        ))
        .await;
    assert_eq!(response.status, 200, "{:?}", response.body);
    assert_response("projects.commands", response.status, &response.body);
    let commands = response.body["commands"].as_array().unwrap();
    let row = |name: &str| commands.iter().find(|row| row["name"] == name);
    let review = row("review").unwrap_or_else(|| panic!("no review row: {commands:#?}"));
    assert_eq!(review["source"], "command");
    assert_eq!(review["origin"], "builtin");
    assert_eq!(review["argumentHint"], "arguments");
    let skill = row("skill:omp-only").unwrap_or_else(|| panic!("no skill row: {commands:#?}"));
    assert_eq!(skill["source"], "skill");
    assert_eq!(skill["origin"], "user");
    assert!(
        row("autocompact").is_none(),
        "pi's scan must not stand in for OMP's menu: {commands:#?}"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(300), fixture.requests.recv())
            .await
            .is_err(),
        "an OMP listing must not ask the host for pi's scan"
    );

    // The offer is only as durable as the host reporting it: once that machine
    // is gone, OMP is no longer an agent this workspace's host runs, so a
    // command query for it is refused again.
    fixture.state.forget_host_providers(&fixture.host_id);
    let gone = fixture
        .get(&format!(
            "/api/v1/projects/{}/commands?provider=omp&environmentId={}",
            fixture.project_id, environment.id
        ))
        .await;
    assert_status_and_error(&gone, 400, "invalid_request");
}

/// A discovered agent that has not run in this workspace answers empty.
///
/// There is no scan to fall back to and another agent's menu would be a lie, so
/// an empty list is the honest answer until a session advertises one.
#[tokio::test]
async fn project_commands_is_empty_for_a_discovered_provider_without_a_session() {
    let mut fixture = fixture().await;
    fixture.state.record_host_providers(
        &fixture.host_id,
        vec![
            ProviderSpec::pi(),
            discovered_provider("omp", "/usr/bin/omp"),
        ],
    );

    let response = fixture
        .get(&format!(
            "/api/v1/projects/{}/commands?provider=omp",
            fixture.project_id
        ))
        .await;
    assert_eq!(response.status, 200, "{:?}", response.body);
    assert_response("projects.commands", response.status, &response.body);
    assert_eq!(
        response.body["commands"].as_array().map(Vec::len),
        Some(0),
        "no pi rows may leak into an unscanned agent's menu: {:?}",
        response.body
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(300), fixture.requests.recv())
            .await
            .is_err(),
        "a discovered agent with no advertisement must not ask the host"
    );
}

/// A live session's advertisement is merged over the workspace scan: it can
/// name commands the scan cannot see, while the scan supplies the origin and
/// hint ACP's advertisement does not carry.
#[tokio::test]
async fn project_commands_merges_the_live_advertisement_over_the_scan() {
    let mut fixture = fixture().await;
    fixture.answer_rpc(HostRpcOutcome::Result {
        result: json!({
            "commands": [
                { "name": "compact", "origin": "builtin", "description": "Compact", "argumentHint": null },
                { "name": "review", "origin": "project", "description": "Review the diff", "argumentHint": "[range]" },
            ]
        }),
    });
    // A session in this workspace advertised its own list. `review` is already
    // answered by the scan and keeps the scan's row; `skill:search` and a
    // package prompt exist only in the advertisement.
    fixture.state.commands.record(
        &fixture.host_id,
        &fixture.provider,
        WORKSPACE,
        vec![
            ProviderCommand {
                name: "review".into(),
                description: "Review the diff (live)".into(),
                argument_hint: Some("[live range]".into()),
            },
            ProviderCommand {
                name: "skill:search".into(),
                description: "Web search".into(),
                argument_hint: None,
            },
            ProviderCommand {
                name: "package-prompt".into(),
                description: "Shipped by a package".into(),
                argument_hint: None,
            },
        ],
    );

    let provider = fixture.provider.clone();
    let response = fixture
        .get(&format!(
            "/api/v1/projects/{}/commands?provider={provider}",
            fixture.project_id
        ))
        .await;
    assert_eq!(response.status, 200);
    assert_response("projects.commands", response.status, &response.body);
    let commands = response.body["commands"].as_array().unwrap();
    let row = |name: &str| {
        commands
            .iter()
            .find(|row| row["name"] == name)
            .unwrap_or_else(|| panic!("no {name} row: {commands:#?}"))
    };
    // The scan's row wins for a name it already answered.
    assert_eq!(row("review")["origin"], "project");
    assert_eq!(row("review")["argumentHint"], "[range]");
    // A skill is attributed by its name and the contract's `skill` source.
    assert_eq!(row("skill:search")["source"], "skill");
    assert_eq!(row("skill:search")["origin"], "user");
    // Anything else the agent advertises beyond the scan is the agent's own.
    assert_eq!(row("package-prompt")["source"], "command");
    assert_eq!(row("package-prompt")["origin"], "builtin");
    assert_eq!(row("package-prompt")["description"], "Shipped by a package");

    match fixture.next_rpc_request().await {
        HostRpcOperation::ListCommands { cwd } => assert_eq!(cwd, WORKSPACE),
        other => panic!("expected a command listing, got {other:?}"),
    }
}

#[tokio::test]
async fn a_host_that_omits_its_command_list_is_a_gateway_error() {
    let mut fixture = fixture().await;
    fixture.answer_rpc(HostRpcOutcome::Result {
        result: json!({ "unexpected": true }),
    });
    let provider = fixture.provider.clone();
    let response = fixture
        .get(&format!(
            "/api/v1/projects/{}/commands?provider={provider}",
            fixture.project_id
        ))
        .await;
    assert_status_and_error(&response, 502, "host_unavailable");
    let _ = fixture.next_rpc_request().await;
}

/* ------------------------------------------------------------------ */
/* Attachments                                                         */
/* ------------------------------------------------------------------ */

#[tokio::test]
async fn uploading_an_attachment_writes_inside_the_attachment_root() {
    let mut fixture = fixture().await;
    fixture.answer_file(HostFileOutcome::Written(HostFileContent {
        path: format!("{}/notes.txt", fixture.attachments_root),
        content: String::new(),
        content_encoding: HostFileEncoding::Utf8,
        size_bytes: 5,
        mime_type: Some("text/plain".into()),
        modified_at_ms: None,
        sha256: None,
    }));

    let response = upload(
        &fixture.addr,
        &format!("/api/v1/projects/{}/attachments", fixture.project_id),
        "file",
        "notes.txt",
        b"hello",
    )
    .await;
    assert_eq!(response.status, 201, "{:?}", response.body);
    assert_response("projects.uploadAttachment", response.status, &response.body);
    assert_eq!(response.body["type"], "localFile");
    assert_eq!(response.body["name"], "notes.txt");
    assert_eq!(response.body["sizeBytes"], 5);

    match fixture.next_file_request().await {
        HostFileOperation::Write {
            path,
            root_path,
            content,
            max_bytes,
            overwrite,
        } => {
            assert_eq!(path, format!("{}/notes.txt", fixture.attachments_root));
            assert_eq!(root_path, fixture.attachments_root);
            // The bytes are base64 so binary uploads survive the JSON hop.
            assert_eq!(content, "aGVsbG8=");
            assert!(max_bytes > 0);
            assert!(!overwrite, "an upload must not clobber silently");
        }
        other => panic!("expected a write, got {other:?}"),
    }
}

#[tokio::test]
async fn an_upload_reports_the_path_the_host_actually_wrote() {
    let mut fixture = fixture().await;
    // The worker suffixes a colliding name, so the response's `path` and `name`
    // are the file's real identity — a client that kept the requested name would
    // reference a file that is not there.
    fixture.answer_file(HostFileOutcome::Written(HostFileContent {
        path: format!("{}/notes-2.txt", fixture.attachments_root),
        content: String::new(),
        content_encoding: HostFileEncoding::Utf8,
        size_bytes: 5,
        mime_type: Some("text/plain".into()),
        modified_at_ms: None,
        sha256: None,
    }));

    let response = upload(
        &fixture.addr,
        &format!("/api/v1/projects/{}/attachments", fixture.project_id),
        "file",
        "notes.txt",
        b"hello",
    )
    .await;
    assert_eq!(response.status, 201, "{:?}", response.body);
    assert_response("projects.uploadAttachment", response.status, &response.body);
    assert_eq!(response.body["name"], "notes-2.txt");
    assert!(
        response.body["path"]
            .as_str()
            .unwrap()
            .ends_with("notes-2.txt"),
        "unexpected path: {}",
        response.body["path"]
    );
    let _ = fixture.next_file_request().await;
}

#[tokio::test]
async fn a_write_failure_at_the_host_crosses_over_as_a_contract_error() {
    let mut fixture = fixture().await;
    fixture.answer_file(HostFileOutcome::Failed {
        code: "file_too_large".into(),
        message: "too big".into(),
    });
    let response = upload(
        &fixture.addr,
        &format!("/api/v1/projects/{}/attachments", fixture.project_id),
        "file",
        "huge.bin",
        b"hello",
    )
    .await;
    assert_status_and_error(&response, 413, "file_too_large");
    let _ = fixture.next_file_request().await;
}

#[tokio::test]
async fn an_upload_filename_cannot_escape_the_attachment_root() {
    let mut fixture = fixture().await;
    fixture.answer_file(HostFileOutcome::Written(HostFileContent {
        path: format!("{}/passwd", fixture.attachments_root),
        content: String::new(),
        content_encoding: HostFileEncoding::Utf8,
        size_bytes: 1,
        mime_type: None,
        modified_at_ms: None,
        sha256: None,
    }));

    let response = upload(
        &fixture.addr,
        &format!("/api/v1/projects/{}/attachments", fixture.project_id),
        "file",
        "../../etc/passwd",
        b"x",
    )
    .await;
    assert_eq!(response.status, 201);
    match fixture.next_file_request().await {
        HostFileOperation::Write {
            path, root_path, ..
        } => {
            // Only the final segment survives, so the path stays under the root.
            assert_eq!(path, format!("{}/passwd", fixture.attachments_root));
            assert_eq!(root_path, fixture.attachments_root);
        }
        other => panic!("expected a write, got {other:?}"),
    }

    // A name that reduces to nothing usable is refused outright.
    for bad in ["..", "/", ""] {
        let refused = upload(
            &fixture.addr,
            &format!("/api/v1/projects/{}/attachments", fixture.project_id),
            "file",
            bad,
            b"x",
        )
        .await;
        assert_status_and_error(&refused, 400, "invalid_path");
    }
}

#[tokio::test]
async fn an_upload_without_a_file_part_is_a_bad_request() {
    let mut fixture = fixture().await;
    let response = upload(
        &fixture.addr,
        &format!("/api/v1/projects/{}/attachments", fixture.project_id),
        "not-a-file",
        "x.txt",
        b"x",
    )
    .await;
    assert_status_and_error(&response, 400, "invalid_request");
    assert!(fixture.requests.try_recv().is_err());
}

#[tokio::test]
async fn attachment_content_is_confined_to_the_attachment_root() {
    let mut fixture = fixture().await;
    fixture.answer_file(content(
        &format!("{}/notes.txt", fixture.attachments_root),
        "hello",
    ));
    let response = fixture
        .get(&format!(
            "/api/v1/projects/{}/attachments/content?path={}",
            fixture.project_id,
            urlencode(&format!("{}/notes.txt", fixture.attachments_root))
        ))
        .await;
    assert_eq!(response.status, 200);
    assert_eq!(response.bytes, b"hello");
    match fixture.next_file_request().await {
        HostFileOperation::Read {
            path, root_path, ..
        } => {
            assert_eq!(path, format!("{}/notes.txt", fixture.attachments_root));
            assert_eq!(
                root_path.as_deref(),
                Some(fixture.attachments_root.as_str())
            );
        }
        other => panic!("expected a read, got {other:?}"),
    }

    // A relative path is not a host path and is refused before any request.
    let relative = fixture
        .get(&format!(
            "/api/v1/projects/{}/attachments/content?path=notes.txt",
            fixture.project_id
        ))
        .await;
    assert_status_and_error(&relative, 400, "invalid_path");
}

#[tokio::test]
async fn a_host_without_a_data_directory_cannot_locate_attachments() {
    let fixture = fixture().await;
    // A second project on a host that never reported a data directory.
    let (host, _) = fixture
        .state
        .registry
        .register_host("bare".into(), loom_relay::now_ms())
        .unwrap();
    assert!(host.data_dir.is_none());
    let (project, _) = fixture
        .state
        .registry
        .create_project(
            "bare".into(),
            ProjectKind::Standard,
            None,
            loom_relay::now_ms(),
        )
        .unwrap();
    fixture
        .state
        .registry
        .add_project_source(
            &project.id,
            host.id,
            "/srv/bare".into(),
            None,
            loom_relay::now_ms(),
        )
        .unwrap();

    let response = fixture
        .get(&format!(
            "/api/v1/projects/{}/attachments/content?path=/x/y",
            project.id
        ))
        .await;
    assert_status_and_error(&response, 501, "not_configured");
}

#[tokio::test]
async fn copying_attachments_confines_both_projects() {
    let mut fixture = fixture().await;
    let (source, _) = fixture
        .state
        .registry
        .create_project(
            "source".into(),
            ProjectKind::Standard,
            None,
            loom_relay::now_ms(),
        )
        .unwrap();
    fixture
        .state
        .registry
        .add_project_source(
            &source.id,
            fixture.host_id.clone(),
            "/srv/source".into(),
            None,
            loom_relay::now_ms(),
        )
        .unwrap();
    let source_root = project_attachments_root(DATA_DIR, &source.id.to_string());

    fixture.answer_file(HostFileOutcome::Copied {
        files: vec![HostFileContent {
            path: format!("{}/a.png", fixture.attachments_root),
            content: String::new(),
            content_encoding: HostFileEncoding::Utf8,
            size_bytes: 3,
            mime_type: Some("image/png".into()),
            modified_at_ms: None,
            sha256: None,
        }],
        failures: vec![HostFileFailure {
            path: format!("{source_root}/gone.png"),
            code: "not_found".into(),
            message: "no such file".into(),
        }],
    });

    let response = fixture
        .post(
            &format!("/api/v1/projects/{}/attachments/copy", fixture.project_id),
            Some(json!({
                "sourceProjectId": source.id.to_string(),
                "paths": [
                    format!("{source_root}/a.png"),
                    format!("{source_root}/gone.png"),
                ],
            })),
        )
        .await;
    assert_eq!(response.status, 200, "{:?}", response.body);
    assert_response("projects.copyAttachments", response.status, &response.body);
    assert_eq!(response.body["ok"], true);

    match fixture.next_file_request().await {
        HostFileOperation::Copy {
            paths,
            source_root: requested_source,
            destination,
            destination_root,
            ..
        } => {
            assert_eq!(paths.len(), 2);
            assert_eq!(requested_source, source_root);
            assert_eq!(destination, fixture.attachments_root);
            assert_eq!(destination_root, fixture.attachments_root);
        }
        other => panic!("expected a copy, got {other:?}"),
    }
}

#[tokio::test]
async fn copying_from_an_unknown_or_foreign_project_is_refused() {
    let fixture = fixture().await;
    let unknown = fixture
        .post(
            &format!("/api/v1/projects/{}/attachments/copy", fixture.project_id),
            Some(json!({
                "sourceProjectId": UNKNOWN_PROJECT,
                "paths": ["/var/lib/loom/project-attachments/x/a.png"],
            })),
        )
        .await;
    assert_status_and_error(&unknown, 404, "project_not_found");

    let empty = fixture
        .post(
            &format!("/api/v1/projects/{}/attachments/copy", fixture.project_id),
            Some(json!({
                "sourceProjectId": fixture.project_id,
                "paths": [],
            })),
        )
        .await;
    // The contract's own middleware rejects an empty list before the handler.
    assert_eq!(empty.status, 422);
    assert_middleware_error(&empty.body);
}

/* ------------------------------------------------------------------ */
/* Prompt history                                                      */
/* ------------------------------------------------------------------ */

#[tokio::test]
async fn project_prompt_history_aggregates_across_the_projects_threads() {
    let fixture = fixture().await;
    let now = loom_relay::now_ms();
    let (environment, _) = fixture
        .state
        .registry
        .create_environment(
            Some(fixture.project_id.parse().unwrap()),
            fixture.host_id.clone(),
            EnvironmentKind::Unmanaged,
            Some(WORKSPACE.into()),
            now,
        )
        .unwrap();
    let (thread, _) = fixture
        .state
        .registry
        .create_thread(
            Some(fixture.project_id.parse().unwrap()),
            Some("t".into()),
            Some(environment.id),
            now,
        )
        .unwrap();
    // `projects.promptHistory` reads the thread's log, so the events have to be
    // published the way the message route publishes them.
    for (role, text, at) in [
        (loom_domain::MessageRole::User, "first prompt", now + 1),
        (loom_domain::MessageRole::Assistant, "an answer", now + 2),
        (loom_domain::MessageRole::User, "second prompt", now + 3),
    ] {
        let events = fixture
            .state
            .registry
            .post_message(&thread.id, role, text.into(), at)
            .unwrap();
        for event in &events {
            fixture.state.publish_domain_event(event).unwrap();
        }
    }

    let response = fixture
        .get(&format!(
            "/api/v1/projects/{}/prompt-history",
            fixture.project_id
        ))
        .await;
    assert_eq!(response.status, 200);
    assert_response("projects.promptHistory", response.status, &response.body);
    let prompts = response.body.as_array().unwrap();
    // Only user messages, newest first.
    assert_eq!(prompts.len(), 2);
    assert_eq!(prompts[0]["input"][0]["text"], "second prompt");
    assert_eq!(prompts[0]["input"][0]["type"], "text");
    assert_eq!(prompts[1]["input"][0]["text"], "first prompt");

    let limited = fixture
        .get(&format!(
            "/api/v1/projects/{}/prompt-history?limit=1",
            fixture.project_id
        ))
        .await;
    assert_eq!(limited.status, 200);
    assert_response("projects.promptHistory", limited.status, &limited.body);
    assert_eq!(limited.body.as_array().unwrap().len(), 1);

    let unknown = fixture
        .get(&format!(
            "/api/v1/projects/{UNKNOWN_PROJECT}/prompt-history"
        ))
        .await;
    assert_status_and_error(&unknown, 404, "project_not_found");
}

/* ------------------------------------------------------------------ */
/* Reorder, source update, delete                                       */
/* ------------------------------------------------------------------ */

#[tokio::test]
async fn project_reorder_moves_a_project_between_neighbours() {
    let fixture = fixture().await;
    let now = loom_relay::now_ms();
    let (second, _) = fixture
        .state
        .registry
        .create_project("second".into(), ProjectKind::Standard, None, now)
        .unwrap();
    let (third, _) = fixture
        .state
        .registry
        .create_project("third".into(), ProjectKind::Standard, None, now)
        .unwrap();
    let personal: loom_domain::ProjectId = fixture.personal_project_id.parse().unwrap();

    let response = fixture
        .patch(
            &format!("/api/v1/projects/{}/order", third.id),
            json!({
                "previousProjectId": personal.to_string(),
                "nextProjectId": second.id.to_string(),
            }),
        )
        .await;
    assert_eq!(response.status, 200, "{:?}", response.body);
    assert_response("projects.reorder", response.status, &response.body);
    let names = response
        .body
        .as_array()
        .unwrap()
        .iter()
        .map(|project| project["name"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    // The response is the whole reordered list, which is what a sidebar applies.
    assert_eq!(names, vec!["Personal", "third", "b7", "second"]);
    assert_eq!(
        response.body[0]["id"].as_str().unwrap(),
        personal.to_string()
    );

    // A stale neighbour is a conflict, not a silent no-op.
    let stale = fixture
        .patch(
            &format!("/api/v1/projects/{}/order", third.id),
            json!({
                "previousProjectId": UNKNOWN_PROJECT,
                "nextProjectId": null,
            }),
        )
        .await;
    assert_status_and_error(&stale, 409, "conflict");

    let unknown = fixture
        .patch(
            &format!("/api/v1/projects/{UNKNOWN_PROJECT}/order"),
            json!({ "previousProjectId": null, "nextProjectId": null }),
        )
        .await;
    assert_status_and_error(&unknown, 404, "project_not_found");
}

/// The personal project is a scope with a reserved id, not a listed project.
///
/// The product app addresses the projectless scope by the literal
/// `proj_personal`: it is what a projectless thread is filed under, and what
/// the client puts in its `/threads/:id` route. Serving a minted id there meant
/// that route never matched the project the server reported, which the client
/// renders as "Not found" — and an id re-minted on a start without a snapshot
/// also invalidated every project-scoped URL a client already held.
#[tokio::test]
async fn the_personal_scope_carries_its_reserved_id_and_is_not_a_listed_project() {
    let fixture = fixture().await;
    let personal = fixture.personal_project_id.clone();
    assert_eq!(personal, "proj_personal");

    // A client is handed the scope as `personalProject`...
    let bootstrap = fixture.get("/api/v1/sidebar-bootstrap").await;
    assert_eq!(bootstrap.status, 200, "{:?}", bootstrap.body);
    assert_eq!(bootstrap.body["personalProject"]["id"], personal.as_str());
    assert_eq!(bootstrap.body["personalProject"]["kind"], "personal");
    // ...and it is not one of the rows a client can drag.
    assert!(
        bootstrap.body["projects"]
            .as_array()
            .unwrap()
            .iter()
            .all(|project| project["id"].as_str() != Some(personal.as_str())),
        "{}",
        bootstrap.body
    );

    let listed = fixture.get("/api/v1/projects").await;
    assert_eq!(listed.status, 200, "{:?}", listed.body);
    assert!(
        listed
            .body
            .as_array()
            .unwrap()
            .iter()
            .all(|project| project["id"].as_str() != Some(personal.as_str())),
        "{}",
        listed.body
    );

    // The literal a client sends back resolves...
    let project_path = format!("/api/v1/projects/{personal}");
    let resolved = fixture.get(&project_path).await;
    assert_eq!(resolved.status, 200, "{:?}", resolved.body);
    assert_eq!(resolved.body["id"], personal.as_str());

    // ...and a thread filed under it reports it, so the client's projectless
    // route matches the thread instead of rendering "Not found".
    let (thread, _) = fixture
        .state
        .registry
        .create_thread(
            Some(personal.parse().unwrap()),
            Some("personal thread".into()),
            None,
            loom_relay::now_ms(),
        )
        .unwrap();
    let thread_id = thread.id.to_string();
    let thread_path = format!("/api/v1/threads/{thread_id}");
    let detail = fixture.get(&thread_path).await;
    assert_eq!(detail.status, 200, "{:?}", detail.body);
    assert_eq!(detail.body["projectId"], personal.as_str());

    // The scope still groups its own threads in the sidebar.
    let bootstrap = fixture.get("/api/v1/sidebar-bootstrap").await;
    assert!(
        bootstrap.body["personalProject"]["threads"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["id"].as_str() == Some(thread_id.as_str())),
        "{}",
        bootstrap.body
    );

    fixture.state.shutdown().unwrap();
}

#[tokio::test]
async fn project_update_source_repoints_and_promotes() {
    let fixture = fixture().await;
    let project_id = fixture.project_id.parse().unwrap();
    let (project, _) = fixture
        .state
        .registry
        .add_project_source(
            &project_id,
            fixture.host_id.clone(),
            "/srv/other".into(),
            None,
            loom_relay::now_ms(),
        )
        .unwrap();
    let second = project.sources[1].id.clone();
    assert!(!project.sources[1].is_default);

    let response = fixture
        .patch(
            &format!("/api/v1/projects/{}/sources/{}", fixture.project_id, second),
            json!({ "type": "local_path", "path": "/srv/repointed", "isDefault": true }),
        )
        .await;
    assert_eq!(response.status, 200, "{:?}", response.body);
    assert_response("projects.updateSource", response.status, &response.body);
    assert_eq!(response.body["path"], "/srv/repointed");
    assert_eq!(response.body["isDefault"], true);
    assert_eq!(response.body["type"], "local_path");

    // The invariant holds: exactly one default, and it is the promoted one.
    let updated = fixture.state.registry.project(&project_id).unwrap();
    assert_eq!(
        updated
            .sources
            .iter()
            .filter(|source| source.is_default)
            .count(),
        1
    );
    assert_eq!(updated.sources[1].id, second);

    // An unknown source is a `404`.
    let unknown_source = fixture
        .patch(
            &format!(
                "/api/v1/projects/{}/sources/src_01M27Y6Q0J8V4W2C7K5N3P1R9Z",
                fixture.project_id
            ),
            json!({ "type": "local_path" }),
        )
        .await;
    assert_status_and_error(&unknown_source, 404, "not_found");

    // The wrong `type` is refused by the contract's middleware.
    let wrong_type = fixture
        .patch(
            &format!("/api/v1/projects/{}/sources/{}", fixture.project_id, second),
            json!({ "type": "remote" }),
        )
        .await;
    assert_eq!(wrong_type.status, 422);
    assert_middleware_error(&wrong_type.body);

    // A blank path is a `400` and leaves the source untouched.
    let blank = fixture
        .patch(
            &format!("/api/v1/projects/{}/sources/{}", fixture.project_id, second),
            json!({ "type": "local_path", "path": "   " }),
        )
        .await;
    assert_status_and_error(&blank, 400, "invalid_request");
    assert_eq!(
        fixture.state.registry.project(&project_id).unwrap().sources[1].path,
        "/srv/repointed"
    );
}

#[tokio::test]
async fn deleting_a_project_refuses_while_it_holds_live_work() {
    let fixture = fixture().await;
    let now = loom_relay::now_ms();
    let (environment, _) = fixture
        .state
        .registry
        .create_environment(
            Some(fixture.project_id.parse().unwrap()),
            fixture.host_id.clone(),
            EnvironmentKind::Unmanaged,
            Some(WORKSPACE.into()),
            now,
        )
        .unwrap();
    let (thread, _) = fixture
        .state
        .registry
        .create_thread(
            Some(fixture.project_id.parse().unwrap()),
            Some("live".into()),
            Some(environment.id.clone()),
            now,
        )
        .unwrap();

    let busy = fixture
        .delete(&format!("/api/v1/projects/{}", fixture.project_id), None)
        .await;
    assert_status_and_error(&busy, 409, "conflict");
    // Refuse, never cascade: nothing moved.
    assert!(!fixture
        .state
        .registry
        .project(&fixture.project_id.parse().unwrap())
        .unwrap()
        .is_deleted());

    // Archiving the thread and destroying the environment clears the way.
    fixture
        .state
        .registry
        .archive_thread(&thread.id, now)
        .unwrap();
    fixture
        .state
        .registry
        .delete_environment(&environment.id, now)
        .unwrap();

    let deleted = fixture
        .delete(&format!("/api/v1/projects/{}", fixture.project_id), None)
        .await;
    assert_eq!(deleted.status, 200, "{:?}", deleted.body);
    assert_response("projects.delete", deleted.status, &deleted.body);
    assert_eq!(deleted.body["ok"], true);
    // The tombstone is invisible to the list and to a second delete.
    let listed = fixture.get("/api/v1/projects").await;
    assert!(listed
        .body
        .as_array()
        .unwrap()
        .iter()
        .all(|project| project["id"] != fixture.project_id));
    let again = fixture
        .delete(&format!("/api/v1/projects/{}", fixture.project_id), None)
        .await;
    assert_status_and_error(&again, 404, "project_not_found");
}

/* ------------------------------------------------------------------ */
/* Thread sections                                                     */
/* ------------------------------------------------------------------ */

#[tokio::test]
async fn thread_sections_are_created_renamed_listed_and_deleted() {
    let fixture = fixture().await;

    let created = fixture
        .post(
            "/api/v1/thread-sections",
            Some(json!({ "name": "  Backlog  " })),
        )
        .await;
    assert_eq!(created.status, 201, "{:?}", created.body);
    assert_response("threadSections.create", created.status, &created.body);
    assert_eq!(created.body["name"], "Backlog");
    let section_id = created.body["id"].as_str().unwrap().to_owned();

    // A duplicate name is the contract's `409`.
    let duplicate = fixture
        .post(
            "/api/v1/thread-sections",
            Some(json!({ "name": "Backlog" })),
        )
        .await;
    assert_status_and_error(&duplicate, 409, "section_name_conflict");

    // An empty name is refused by the contract's own middleware.
    let empty = fixture
        .post("/api/v1/thread-sections", Some(json!({ "name": "" })))
        .await;
    assert_eq!(empty.status, 422);
    assert_middleware_error(&empty.body);

    // A rename is visible in the sidebar bootstrap's `sections`.
    let renamed = fixture
        .patch(
            "/api/v1/thread-sections",
            json!({ "id": section_id, "name": "Later" }),
        )
        .await;
    assert_eq!(renamed.status, 200, "{:?}", renamed.body);
    assert_response("threadSections.update", renamed.status, &renamed.body);
    assert_eq!(renamed.body["name"], "Later");
    assert_eq!(renamed.body["updatedThreadCount"], 0);

    let bootstrap = fixture.get("/api/v1/sidebar-bootstrap").await;
    assert_eq!(bootstrap.status, 200);
    assert_response(
        "projects.sidebarBootstrap",
        bootstrap.status,
        &bootstrap.body,
    );
    assert_eq!(bootstrap.body["sections"][0]["name"], "Later");

    // A second section cannot take the first one's name.
    let other = fixture
        .post("/api/v1/thread-sections", Some(json!({ "name": "Other" })))
        .await;
    assert_eq!(other.status, 201);
    let other_id = other.body["id"].as_str().unwrap().to_owned();
    let clash = fixture
        .patch(
            "/api/v1/thread-sections",
            json!({ "id": other_id, "name": "Later" }),
        )
        .await;
    assert_status_and_error(&clash, 409, "section_name_conflict");

    // Deleting a section a thread still references reports the count and does
    // **not** rewrite the thread.
    let now = loom_relay::now_ms();
    let (thread, _) = fixture
        .state
        .registry
        .create_thread(
            Some(fixture.project_id.parse().unwrap()),
            Some("filed".into()),
            None,
            now,
        )
        .unwrap();
    fixture
        .state
        .registry
        .update_thread(
            &thread.id,
            &loom_domain::ThreadUpdate {
                section_id: Some(Some(section_id.clone())),
                ..Default::default()
            },
            now,
        )
        .unwrap();

    let deleted = fixture
        .delete("/api/v1/thread-sections", Some(json!({ "id": section_id })))
        .await;
    assert_eq!(deleted.status, 200, "{:?}", deleted.body);
    assert_response("threadSections.delete", deleted.status, &deleted.body);
    assert_eq!(deleted.body["updatedThreadCount"], 1);
    assert_eq!(
        fixture
            .state
            .registry
            .thread(&thread.id)
            .unwrap()
            .section_id
            .as_deref(),
        Some(section_id.as_str()),
        "a section delete must not re-file the threads that referenced it"
    );

    // Deleting it again is the contract's `404`.
    let again = fixture
        .delete("/api/v1/thread-sections", Some(json!({ "id": section_id })))
        .await;
    assert_status_and_error(&again, 404, "section_not_found");

    let unknown = fixture
        .patch(
            "/api/v1/thread-sections",
            json!({ "id": UNKNOWN_SECTION, "name": "x" }),
        )
        .await;
    assert_status_and_error(&unknown, 404, "section_not_found");
}

/* ------------------------------------------------------------------ */
/* Restart recovery                                                    */
/* ------------------------------------------------------------------ */

#[tokio::test]
async fn sections_projects_and_their_order_survive_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let config = AppConfig {
        backend_path: Some(dir.path().to_path_buf()),
        // The background sweeper would race the snapshot this test writes by
        // hand; the test drives reconciliation itself.
        reconcile_interval: Duration::ZERO,
        entity_write_interval: Duration::ZERO,
        ..AppConfig::default()
    };

    let section_id;
    let third_id;
    let personal_id;
    {
        let state = AppState::build(config.clone()).unwrap();
        let (section, _) = state
            .registry
            .create_thread_section("Backlog".into(), loom_relay::now_ms())
            .unwrap();
        section_id = section.id.to_string();
        personal_id = state.registry.personal_project_id().to_string();
        let (second, _) = state
            .registry
            .create_project(
                "second".into(),
                ProjectKind::Standard,
                None,
                loom_relay::now_ms(),
            )
            .unwrap();
        let (third, _) = state
            .registry
            .create_project(
                "third".into(),
                ProjectKind::Standard,
                None,
                loom_relay::now_ms(),
            )
            .unwrap();
        third_id = third.id.to_string();
        state
            .registry
            .reorder_project(
                &third.id,
                Some(&state.registry.personal_project_id()),
                Some(&second.id),
                loom_relay::now_ms(),
            )
            .unwrap();
        // A deleted project must stay deleted across the restart too.
        let (doomed, _) = state
            .registry
            .create_project(
                "doomed".into(),
                ProjectKind::Standard,
                None,
                loom_relay::now_ms(),
            )
            .unwrap();
        state
            .registry
            .delete_project(&doomed.id, loom_relay::now_ms())
            .unwrap();
        state.write_entity_view().unwrap();
        state.shutdown().unwrap();
    }

    let state = AppState::build(config).unwrap();
    let sections = state.registry.thread_sections();
    assert_eq!(sections.len(), 1);
    assert_eq!(sections[0].id.to_string(), section_id);
    assert_eq!(sections[0].name, "Backlog");

    let projects = state.registry.projects();
    let ids = projects
        .iter()
        .map(|project| project.id.to_string())
        .collect::<Vec<_>>();
    // `third` moved ahead of `second`, and the tombstone did not come back.
    assert_eq!(
        ids.len(),
        3,
        "the deleted project must stay out of the list"
    );
    assert_eq!(ids[0], personal_id);
    assert_eq!(ids[1], third_id.clone());
    let listed = state
        .registry
        .all_projects()
        .into_iter()
        .filter(|project| project.is_deleted())
        .count();
    assert_eq!(listed, 1, "the deleted project must stay a tombstone");
    state.shutdown().unwrap();
}

/* ------------------------------------------------------------------ */
/* Helpers                                                             */
/* ------------------------------------------------------------------ */

/// Percent-encodes the characters that would otherwise split a query string.
fn urlencode(raw: &str) -> String {
    let mut encoded = String::with_capacity(raw.len());
    for byte in raw.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(byte as char)
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}
