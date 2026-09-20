//! B6 integration coverage: HTTP routes must ask the owning host for workspace
//! state, and environment lifecycle changes must remain serialized.

use std::future::Future;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use loom_contract::shared;
use loom_domain::{EnvironmentKind, HostId, ThreadTrigger};
use loom_provider_protocol::{HostRpcOperation, HostRpcOutcome, HostRpcReport, HostRpcRequest};
use loom_server::http::router;
use loom_server::state::{AppConfig, AppState};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;

const TIMEOUT: Duration = Duration::from_secs(10);

struct Response {
    status: u16,
    body: Value,
}

async fn spawn_server() -> (String, AppState, JoinHandle<()>) {
    let state = AppState::build(AppConfig::default()).unwrap();
    let app = router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (
        format!("{}:{}", address.ip(), address.port()),
        state,
        server,
    )
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
    let head = String::from_utf8_lossy(&raw[..separator]);
    let status = head
        .split(' ')
        .nth(1)
        .and_then(|value| value.parse().ok())
        .unwrap();
    Response {
        status,
        body: serde_json::from_slice(&raw[separator..]).unwrap_or(Value::Null),
    }
}

#[track_caller]
fn assert_response(route_id: &str, status: u16, body: &Value) {
    let contract = shared();
    let route = contract.route_by_id(route_id).unwrap();
    let violations = contract.validate_response(route, status, body);
    assert!(
        violations.is_empty(),
        "{route_id} returned {status} outside the contract: {}\n{body}",
        loom_contract::describe(&violations)
    );
}

#[track_caller]
fn assert_error(status: u16, body: &Value) {
    let contract = shared();
    assert!(
        contract.validate_error_body(body).is_empty(),
        "invalid API error body: {body}"
    );
    let code = body["code"].as_str().unwrap();
    assert!(
        contract.error_statuses(code).contains(&u64::from(status)),
        "{code} is not declared at HTTP {status}"
    );
}

struct Fixture {
    addr: String,
    state: AppState,
    environment_id: String,
    thread_id: String,
    requests: mpsc::UnboundedReceiver<HostRpcOperation>,
    host: JoinHandle<()>,
    server: JoinHandle<()>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.state.shutdown().unwrap();
        self.host.abort();
        self.server.abort();
    }
}

impl Fixture {
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

    fn delete(&self, path: &str) -> impl Future<Output = Response> + '_ {
        let addr = self.addr.clone();
        let path = path.to_owned();
        async move { request(&addr, "DELETE", &path, None).await }
    }

    async fn next_request(&mut self) -> HostRpcOperation {
        tokio::time::timeout(TIMEOUT, self.requests.recv())
            .await
            .expect("the HTTP route never asked the host")
            .expect("the scripted host closed")
    }
}

async fn fixture() -> Fixture {
    let (addr, state, server) = spawn_server().await;
    let (requests_tx, requests) = mpsc::unbounded_channel();
    let (host_id, host) = spawn_scripted_host(&addr, requests_tx).await;
    let now = loom_relay::now_ms();
    let (project, _) = state
        .registry
        .create_project("b6".into(), loom_domain::ProjectKind::Standard, None, now)
        .unwrap();
    let (environment, _) = state
        .registry
        .create_environment(
            Some(project.id),
            host_id,
            EnvironmentKind::Unmanaged,
            Some("/srv/b6".into()),
            now,
        )
        .unwrap();
    let (thread, _) = state
        .registry
        .create_thread(
            Some(environment.project_id.clone()),
            Some("b6 thread".into()),
            Some(environment.id.clone()),
            now,
        )
        .unwrap();
    Fixture {
        addr,
        state,
        environment_id: environment.id.to_string(),
        thread_id: thread.id.to_string(),
        requests,
        host,
        server,
    }
}

async fn spawn_scripted_host(
    addr: &str,
    requests: mpsc::UnboundedSender<HostRpcOperation>,
) -> (HostId, JoinHandle<()>) {
    let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/internal/ws"))
        .await
        .unwrap();
    let welcome = recv_value(&mut socket).await;
    assert_eq!(welcome["type"], "hello");
    socket
        .send(Message::Text(
            json!({ "type": "enroll_host", "name": "b6-scripted" })
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    let enrolled = recv_value(&mut socket).await;
    assert_eq!(enrolled["type"], "host_enrolled", "{enrolled}");
    let host_id: HostId = enrolled["host"]["id"].as_str().unwrap().parse().unwrap();
    socket
        .send(Message::Text(
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

    let task_host_id = host_id.clone();
    let task = tokio::spawn(async move {
        while let Some(Ok(message)) = socket.next().await {
            let frame = match message {
                Message::Text(text) => serde_json::from_str::<Value>(text.as_str()).unwrap(),
                Message::Binary(bytes) => serde_json::from_slice::<Value>(&bytes).unwrap(),
                _ => continue,
            };
            if frame["type"] != "event" {
                continue;
            }
            let payload = frame["payload"].as_str().unwrap();
            let Ok(request) = serde_json::from_str::<HostRpcRequest>(payload) else {
                continue;
            };
            let operation = request.operation.clone();
            requests.send(operation.clone()).unwrap();
            let outcome = match operation {
                HostRpcOperation::WorkspaceStatus { .. } => HostRpcOutcome::Result {
                    result: json!({
                        "outcome": "available",
                        "workspace": {
                            "workingTree": {
                                "insertions": 1,
                                "deletions": 0,
                                "lineStatsComplete": true,
                                "files": [{
                                    "path": "README.md",
                                    "status": "M",
                                    "insertions": 1,
                                    "deletions": 0
                                }],
                                "hasUncommittedChanges": true,
                                "state": "dirty_uncommitted"
                            },
                            "checkout": {
                                "kind": "branch",
                                "branchName": "main",
                                "headSha": "deadbeef"
                            },
                            "branch": {
                                "currentBranch": "main",
                                "defaultBranch": "main"
                            },
                            "mergeBase": null
                        }
                    }),
                },
                HostRpcOperation::WorkspaceDiff { .. } => HostRpcOutcome::Result {
                    result: json!({
                        "outcome": "available",
                        "diff": {
                            "diff": "diff --git a/README.md b/README.md\n-before\n+after\n",
                            "truncated": false,
                            "shortstat": "1 file changed",
                            "files": "M\tREADME.md\n",
                            "mergeBaseRef": null
                        }
                    }),
                },
                HostRpcOperation::WorkspaceDiffPatch { .. } => HostRpcOutcome::Result {
                    result: json!({
                        "outcome": "available",
                        "patches": [{
                            "path": "README.md",
                            "patch": "-before\n+after\n",
                            "truncated": false
                        }]
                    }),
                },
                HostRpcOperation::WorkspacePullRequest { .. } => HostRpcOutcome::Failed {
                    code: "pull_request_unavailable".into(),
                    message: "no provider is configured".into(),
                },
                _ => HostRpcOutcome::Failed {
                    code: "unknown".into(),
                    message: "the scripted host does not implement this operation".into(),
                },
            };
            let report = HostRpcReport {
                host_id: task_host_id.clone(),
                request_id: request.request_id,
                outcome,
            };
            if socket
                .send(Message::Text(
                    json!({ "type": "host_rpc_report", "report": report })
                        .to_string()
                        .into(),
                ))
                .await
                .is_err()
            {
                break;
            }
        }
    });
    (host_id, task)
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
        Message::Text(text) => serde_json::from_str(text.as_str()).unwrap(),
        Message::Binary(bytes) => serde_json::from_slice(&bytes).unwrap(),
        other => panic!("expected a JSON frame, got {other:?}"),
    }
}

#[test]
fn b6_request_examples_are_contract_shaped() {
    let contract = shared();
    for body in [
        json!({ "action": "commit" }),
        json!({ "action": "pull_request_ready" }),
        json!({ "action": "pull_request_draft" }),
        json!({ "action": "pull_request_merge", "options": { "method": "merge" } }),
    ] {
        assert!(contract
            .validate_request_by_id("environments.actions", &body)
            .is_empty());
    }
    let diff_patch = json!({
        "target": { "type": "uncommitted" },
        "paths": ["README.md"],
    });
    assert!(contract
        .validate_request_by_id("environments.diffPatch", &diff_patch)
        .is_empty());
}

#[tokio::test]
async fn workspace_routes_use_host_rpc_and_return_contract_bodies() {
    let mut fixture = fixture().await;
    let status = fixture
        .get(&format!(
            "/api/v1/environments/{}/status?mergeBaseBranch=main",
            fixture.environment_id
        ))
        .await;
    assert_eq!(status.status, 200);
    assert_response("environments.status", status.status, &status.body);
    assert!(matches!(
        fixture.next_request().await,
        HostRpcOperation::WorkspaceStatus { .. }
    ));

    let diff = fixture
        .get(&format!(
            "/api/v1/environments/{}/diff?target=uncommitted",
            fixture.environment_id
        ))
        .await;
    assert_eq!(diff.status, 200);
    assert_response("environments.diff", diff.status, &diff.body);
    assert!(matches!(
        fixture.next_request().await,
        HostRpcOperation::WorkspaceDiff { .. }
    ));

    let patch = fixture
        .post(
            &format!("/api/v1/environments/{}/diff/patch", fixture.environment_id),
            Some(json!({
                "target": { "type": "uncommitted" },
                "paths": ["README.md"]
            })),
        )
        .await;
    assert_eq!(patch.status, 200);
    assert_response("environments.diffPatch", patch.status, &patch.body);
    assert!(matches!(
        fixture.next_request().await,
        HostRpcOperation::WorkspaceDiffPatch { .. }
    ));
}

#[tokio::test]
async fn pull_request_unavailability_is_explicit_and_lifecycle_is_terminal() {
    let mut fixture = fixture().await;
    let pull_request = fixture
        .get(&format!(
            "/api/v1/environments/{}/pull-request",
            fixture.environment_id
        ))
        .await;
    assert_eq!(pull_request.status, 200);
    assert_response(
        "environments.pullRequest",
        pull_request.status,
        &pull_request.body,
    );
    assert_eq!(pull_request.body["outcome"], "unavailable");
    assert!(matches!(
        fixture.next_request().await,
        HostRpcOperation::WorkspacePullRequest { .. }
    ));

    let invalid_update = fixture
        .patch(
            &format!("/api/v1/environments/{}", fixture.environment_id),
            json!({ "mergeBaseBranch": "feature..bad" }),
        )
        .await;
    assert_eq!(invalid_update.status, 400);
    assert_error(invalid_update.status, &invalid_update.body);
    assert_eq!(invalid_update.body["code"], "invalid_request");
    let environment_id = fixture.environment_id.parse().unwrap();
    assert_eq!(
        fixture
            .state
            .registry
            .environment(&environment_id)
            .unwrap()
            .merge_base_branch,
        None
    );

    let updated = fixture
        .patch(
            &format!("/api/v1/environments/{}", fixture.environment_id),
            json!({ "name": "renamed", "mergeBaseBranch": "main" }),
        )
        .await;
    assert_eq!(updated.status, 200);
    assert_response("environments.update", updated.status, &updated.body);
    assert_eq!(updated.body["name"], "renamed");

    let archived = fixture
        .post(
            &format!(
                "/api/v1/environments/{}/archive-threads",
                fixture.environment_id
            ),
            None,
        )
        .await;
    assert_eq!(archived.status, 200);
    assert_response(
        "environments.archiveThreads",
        archived.status,
        &archived.body,
    );
    let thread_id = fixture.thread_id.parse().unwrap();
    assert_eq!(
        fixture.state.registry.thread(&thread_id).unwrap().status,
        loom_domain::ThreadStatus::Archived
    );

    let deleted = fixture
        .delete(&format!("/api/v1/environments/{}", fixture.environment_id))
        .await;
    assert_eq!(deleted.status, 200);
    assert_response("environments.delete", deleted.status, &deleted.body);
    assert_eq!(deleted.body["ok"], true);

    let fetched = fixture
        .get(&format!("/api/v1/environments/{}", fixture.environment_id))
        .await;
    assert_eq!(fetched.status, 200);
    assert_response("environments.get", fetched.status, &fetched.body);
    assert_eq!(fetched.body["lifecycle"]["phase"], "destroyed");

    let status = fixture
        .get(&format!(
            "/api/v1/environments/{}/status",
            fixture.environment_id
        ))
        .await;
    assert_eq!(status.status, 409);
    assert_error(status.status, &status.body);
    assert_eq!(status.body["code"], "environment_not_ready");
}

#[tokio::test]
async fn archiving_a_working_thread_is_an_atomic_conflict() {
    let fixture = fixture().await;
    let thread_id = fixture.thread_id.parse().unwrap();
    fixture
        .state
        .registry
        .transition_thread(&thread_id, ThreadTrigger::RunStarted, loom_relay::now_ms())
        .unwrap();
    let response = fixture
        .post(
            &format!(
                "/api/v1/environments/{}/archive-threads",
                fixture.environment_id
            ),
            None,
        )
        .await;
    assert_eq!(response.status, 409);
    assert_error(response.status, &response.body);
    assert_eq!(
        fixture.state.registry.thread(&thread_id).unwrap().status,
        loom_domain::ThreadStatus::Working
    );
}
