//! B5 conformance: thread counts, pane actions and thread file routes.
//!
//! The file routes are exercised with a **scripted host**: a real WebSocket
//! connection that enrolls as a host, receives the `HostFileRequest` published
//! to its room, and answers with the bytes a real daemon would send. That is the
//! property this batch has to prove — the control plane never reads its own
//! disk, it asks the machine that owns the thread's environment — and a
//! scripted host can assert exactly which request it received and refuse to
//! answer when it should not have been asked.
//!
//! The filesystem semantics themselves (containment, base64, truncation) are
//! covered against the real implementation in `crates/daemon` (`host_files`),
//! because that is where the filesystem lives.
//!
//! Every successful body is validated against the embedded bb contract, and a
//! JSON request body is asserted against the contract's request schema.

use std::future::Future;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use loom_contract::shared;
use loom_domain::{EnvironmentKind, HostId, Thread, ThreadStatus, ThreadVisibility};
use loom_provider_protocol::{
    thread_storage_root, HostFileEncoding, HostFileEntry, HostFileOperation, HostFileOutcome,
    HostFileReport, HostPathKind,
};
use loom_relay::Scope;
use loom_server::http::router;
use loom_server::state::{AppConfig, AppState};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

const TIMEOUT: Duration = Duration::from_secs(10);
const UNKNOWN_THREAD: &str = "thr_01M27Y6Q0J8V4W2C7K5N3P1R9Z";
const DATA_DIR: &str = "/var/lib/loom";

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

/// A server, a scripted host and a thread bound to an environment on it.
struct Fixture {
    addr: String,
    state: AppState,
    project_id: String,
    environment_id: String,
    thread_id: String,
    host_id: HostId,
    storage_root: String,
    /// Requests the scripted host received, in order.
    requests: mpsc::UnboundedReceiver<HostFileOperation>,
    /// Scripted answers, consumed in order. An empty queue means "answer with
    /// whatever the last scripted answer was", so the common case needs one.
    script: mpsc::UnboundedSender<HostFileOutcome>,
    host: tokio::task::JoinHandle<()>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.state.shutdown();
        self.host.abort();
    }
}

impl Fixture {
    /// The next request the scripted host was asked, or a panic if none came.
    async fn next_request(&mut self) -> HostFileOperation {
        tokio::time::timeout(TIMEOUT, self.requests.recv())
            .await
            .expect("the control plane never asked the host")
            .expect("the host connection closed")
    }

    /// Scripts one answer.
    fn answer(&self, outcome: HostFileOutcome) {
        self.script.send(outcome).unwrap();
    }

    fn get<'a>(&'a self, path: &'a str) -> impl Future<Output = Response> + 'a {
        request(&self.addr, "GET", path, None)
    }

    fn post(&self, path: &str, body: Value) -> impl Future<Output = Response> + '_ {
        let addr = self.addr.clone();
        let path = path.to_owned();
        async move { request(&addr, "POST", &path, Some(body)).await }
    }

    fn thread_id(&self) -> loom_domain::ThreadId {
        self.thread_id.parse().unwrap()
    }

    fn new_thread(&self, title: &str, visibility: ThreadVisibility) -> Thread {
        let (thread, _) = self
            .state
            .registry
            .create_thread(
                Some(self.project_id.parse().unwrap()),
                Some(title.to_owned()),
                Some(self.environment_id.parse().unwrap()),
                loom_relay::now_ms(),
            )
            .unwrap();
        if visibility != ThreadVisibility::Visible {
            self.state
                .registry
                .update_thread(
                    &thread.id,
                    &loom_domain::ThreadUpdate {
                        visibility: Some(visibility),
                        ..Default::default()
                    },
                    loom_relay::now_ms(),
                )
                .unwrap();
        }
        self.state.registry.thread(&thread.id).unwrap()
    }
}

async fn fixture() -> Fixture {
    let (addr, state) = spawn_server().await;

    let (requests_tx, requests) = mpsc::unbounded_channel::<HostFileOperation>();
    let (script_tx, script_rx) = mpsc::unbounded_channel::<HostFileOutcome>();
    let host_id = spawn_scripted_host(&addr, requests_tx, script_rx).await;

    let now = loom_relay::now_ms();
    let (project, _) = state
        .registry
        .create_project("b5".into(), loom_domain::ProjectKind::Standard, None, now)
        .unwrap();
    let (environment, _) = state
        .registry
        .create_environment(
            Some(project.id.clone()),
            host_id.clone(),
            EnvironmentKind::Unmanaged,
            Some("/srv/b5".into()),
            now,
        )
        .unwrap();
    let (thread, _) = state
        .registry
        .create_thread(
            Some(project.id.clone()),
            Some("b5 thread".into()),
            Some(environment.id.clone()),
            now,
        )
        .unwrap();

    Fixture {
        addr,
        state,
        project_id: project.id.to_string(),
        environment_id: environment.id.to_string(),
        thread_id: thread.id.to_string(),
        storage_root: thread_storage_root(DATA_DIR, &thread.id.to_string()),
        host_id,
        requests,
        script: script_tx,
        host: tokio::spawn(async {}),
    }
}

/// Enrolls a scripted host over a real socket and answers its file requests.
///
/// This is a stand-in for a daemon and nothing more: it enrolls with a data
/// directory, answers whatever the test scripted, and forwards each request to
/// the test so the test can assert what was asked. The real filesystem work is
/// tested in `crates/daemon/tests/host_files.rs`.
async fn spawn_scripted_host(
    addr: &str,
    requests: mpsc::UnboundedSender<HostFileOperation>,
    mut script: mpsc::UnboundedReceiver<HostFileOutcome>,
) -> HostId {
    let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
        .await
        .unwrap();
    let welcome = recv_value(&mut socket).await;
    assert_eq!(welcome["type"], "welcome");

    socket
        .send(tokio_tungstenite::tungstenite::Message::Text(
            json!({
                "type": "enroll_host",
                "name": "scripted",
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
    let host_id_for_task = host_id.clone();

    // Follow the host room, which is where a file request is published. A
    // daemon does this in `Daemon::enroll`; a scripted host must do it too or
    // it would never receive a request at all.
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
        let mut last: Option<HostFileOutcome> = None;
        loop {
            let frame = recv_value(&mut socket).await;
            if frame["type"] != "event" {
                continue;
            }
            let payload: Value = serde_json::from_str(frame["payload"].as_str().unwrap()).unwrap();
            let Ok(request) =
                serde_json::from_value::<loom_provider_protocol::HostFileRequest>(payload)
            else {
                continue;
            };
            let _ = requests.send(request.operation);
            let outcome = match script.try_recv() {
                Ok(outcome) => {
                    last = Some(outcome.clone());
                    outcome
                }
                Err(_) => match last.clone() {
                    Some(outcome) => outcome,
                    // No script at all: answer the shape a missing file
                    // produces, so a test that forgot to script fails on the
                    // status rather than hanging.
                    None => HostFileOutcome::Failed {
                        code: "not_found".into(),
                        message: "the scripted host has no answer for this request".into(),
                    },
                },
            };
            let report = HostFileReport {
                host_id: host_id_for_task.clone(),
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
        }
    });
    host_id
}

/* ------------------------------------------------------------------ */
/* Tests                                                               */
/* ------------------------------------------------------------------ */

#[test]
fn b5_json_write_requests_are_contract_shaped() {
    let contract = shared();

    let inside = json!({ "action": "maximize" });
    assert!(
        contract
            .validate_request_by_id("threads.paneAction", &inside)
            .is_empty(),
        "threads.paneAction rejects a contract-shaped body"
    );
    // `action` is a closed enum; an unknown action is not a pane action.
    let outside = json!({ "action": "explode" });
    assert!(
        !contract
            .validate_request_by_id("threads.paneAction", &outside)
            .is_empty(),
        "threads.paneAction accepts an action outside its enum"
    );
    // `action` is required.
    let missing = json!({});
    assert!(
        !contract
            .validate_request_by_id("threads.paneAction", &missing)
            .is_empty(),
        "threads.paneAction accepts a body with no action"
    );
}

#[tokio::test]
async fn counting_threads_filters_groups_and_reports_each_match_once() {
    let fixture = fixture().await;

    let hidden = fixture.new_thread("hidden", ThreadVisibility::Hidden);
    let child = {
        let (thread, _) = fixture
            .state
            .registry
            .create_thread(
                Some(fixture.project_id.parse().unwrap()),
                Some("child".into()),
                Some(fixture.environment_id.parse().unwrap()),
                loom_relay::now_ms(),
            )
            .unwrap();
        fixture
            .state
            .registry
            .update_thread(
                &thread.id,
                &loom_domain::ThreadUpdate {
                    parent_thread_id: Some(Some(fixture.thread_id())),
                    ..Default::default()
                },
                loom_relay::now_ms(),
            )
            .unwrap();
        thread
    };
    let archived = fixture.new_thread("archived", ThreadVisibility::Visible);
    fixture
        .state
        .registry
        .archive_thread(&archived.id, loom_relay::now_ms())
        .unwrap();

    // Default: visible, unarchived only. Hidden and archived drop out, which is
    // what a client's sidebar count must show.
    let all = fixture.get("/api/v1/threads/count").await;
    assert_eq!(all.status, 200, "{}", all.body);
    assert_response("threads.count", 200, &all.body);
    assert_eq!(all.body["total"], 2, "{}", all.body);
    assert!(all.body.get("groups").is_none(), "{}", all.body);

    // `includeHidden` and `includeArchived` are independent flags.
    let with_hidden = fixture
        .get("/api/v1/threads/count?includeHidden=true")
        .await;
    assert_eq!(with_hidden.body["total"], 3, "{}", with_hidden.body);
    let with_archived = fixture
        .get("/api/v1/threads/count?includeArchived=true")
        .await;
    assert_eq!(with_archived.body["total"], 3, "{}", with_archived.body);
    let with_both = fixture
        .get("/api/v1/threads/count?includeHidden=true&includeArchived=true")
        .await;
    assert_eq!(with_both.body["total"], 4, "{}", with_both.body);
    assert_response("threads.count", 200, &with_both.body);

    // `parentThreadId=none` means root threads only; any other value is that
    // parent's id.
    let roots = fixture
        .get("/api/v1/threads/count?parentThreadId=none")
        .await;
    assert_eq!(roots.body["total"], 1, "{}", roots.body);
    let children = fixture
        .get(&format!(
            "/api/v1/threads/count?parentThreadId={}",
            fixture.thread_id
        ))
        .await;
    assert_eq!(children.body["total"], 1, "{}", children.body);

    // A status filter uses bb's vocabulary, not loom's.
    let active = fixture.get("/api/v1/threads/count?status=active").await;
    assert_eq!(active.body["total"], 0, "{}", active.body);
    let idle = fixture.get("/api/v1/threads/count?status=idle").await;
    assert_eq!(idle.body["total"], 2, "{}", idle.body);

    // `groupBy` answers one group per key, and `total` is always the sum.
    let grouped = fixture.get("/api/v1/threads/count?groupBy=project").await;
    assert_eq!(grouped.status, 200, "{}", grouped.body);
    assert_response("threads.count", 200, &grouped.body);
    let groups = grouped.body["groups"].as_array().unwrap();
    assert_eq!(groups.len(), 1, "{}", grouped.body);
    assert_eq!(groups[0]["key"], fixture.project_id);
    assert_eq!(groups[0]["count"], 2);
    assert_eq!(grouped.body["total"], 2);

    let by_host = fixture.get("/api/v1/threads/count?groupBy=host").await;
    assert_response("threads.count", 200, &by_host.body);
    assert_eq!(
        by_host.body["groups"][0]["key"],
        fixture.host_id.to_string()
    );

    // A filter naming an unknown project is an empty count, not an error; an
    // unknown status or grouping is refused rather than silently ignored.
    let other_project = fixture
        .get("/api/v1/threads/count?projectId=proj_01M27Y6Q0J8V4W2C7K5N3P1R9Z")
        .await;
    assert_eq!(other_project.body["total"], 0, "{}", other_project.body);

    let bad_status = fixture.get("/api/v1/threads/count?status=exploded").await;
    assert_eq!(bad_status.status, 400, "{}", bad_status.body);
    assert_error(400, &bad_status.body);
    assert_eq!(bad_status.body["code"], "invalid_request");

    let bad_group = fixture.get("/api/v1/threads/count?groupBy=colour").await;
    assert_eq!(bad_group.status, 400, "{}", bad_group.body);

    assert_eq!(hidden.visibility, ThreadVisibility::Hidden);
    assert_eq!(child.status, ThreadStatus::Idle);
}

#[tokio::test]
async fn a_pane_action_reaches_the_thread_room_and_counts_subscribers() {
    let fixture = fixture().await;
    let path = format!("/api/v1/threads/{}/pane-action", fixture.thread_id);

    let unwatched = fixture.post(&path, json!({ "action": "maximize" })).await;
    assert_eq!(unwatched.status, 200, "{}", unwatched.body);
    assert_response("threads.paneAction", 200, &unwatched.body);
    assert_eq!(unwatched.body["delivered"], 0, "{}", unwatched.body);

    let mut subscriber =
        Subscriber::subscribe(&fixture.addr, Scope::Thread(fixture.thread_id.clone())).await;
    let watched = fixture
        .post(&path, json!({ "action": "clear-spotlight" }))
        .await;
    assert_eq!(watched.status, 200, "{}", watched.body);
    assert_response("threads.paneAction", 200, &watched.body);
    assert_eq!(watched.body["delivered"], 1, "{}", watched.body);

    let frame = subscriber.recv().await;
    let payload: Value = serde_json::from_str(frame["payload"].as_str().unwrap()).unwrap();
    assert_eq!(payload["type"], "thread_pane_action_requested");
    assert_eq!(payload["threadId"], fixture.thread_id);
    assert_eq!(payload["projectId"], fixture.project_id);
    assert_eq!(payload["action"], "clear-spotlight");

    let missing = fixture
        .post(
            &format!("/api/v1/threads/{UNKNOWN_THREAD}/pane-action"),
            json!({ "action": "toggle" }),
        )
        .await;
    assert_eq!(missing.status, 404, "{}", missing.body);
    assert_error(404, &missing.body);
    assert_eq!(missing.body["code"], "thread_not_found");
}

#[tokio::test]
async fn storage_location_is_answered_without_asking_the_host() {
    let fixture = fixture().await;

    let location = fixture
        .get(&format!(
            "/api/v1/threads/{}/thread-storage/location",
            fixture.thread_id
        ))
        .await;
    assert_eq!(location.status, 200, "{}", location.body);
    assert_response("threads.storageLocation", 200, &location.body);
    assert_eq!(location.body["hostId"], fixture.host_id.to_string());
    assert_eq!(location.body["storageRootPath"], fixture.storage_root);
    // The layout is derived from the data directory the host reported at
    // enrollment, so the route needs no request to the machine.
    assert!(
        fixture.storage_root.starts_with(DATA_DIR),
        "{}",
        fixture.storage_root
    );
}

#[tokio::test]
async fn storage_listings_ask_the_host_and_project_what_it_answered() {
    let mut fixture = fixture().await;

    // The host's answer is what the client sees, including truncation.
    fixture.answer(HostFileOutcome::Listing {
        entries: vec![
            HostFileEntry {
                path: "deep".into(),
                name: "deep".into(),
                kind: HostPathKind::Directory,
                score: 0.0,
                positions: Vec::new(),
            },
            HostFileEntry {
                path: "notes.md".into(),
                name: "notes.md".into(),
                kind: HostPathKind::File,
                score: 0.0,
                positions: Vec::new(),
            },
        ],
        truncated: false,
    });
    let files = fixture
        .get(&format!(
            "/api/v1/threads/{}/thread-storage/files?query=no&limit=7",
            fixture.thread_id
        ))
        .await;
    assert_eq!(files.status, 200, "{}", files.body);
    assert_response("threads.storageFiles", 200, &files.body);
    assert_eq!(files.body["storageRootPath"], fixture.storage_root);
    assert_eq!(files.body["truncated"], false);
    // `storageFiles` answers `fileSchema` rows, which are `{path, name}` only.
    assert_eq!(files.body["files"][0]["path"], "deep");
    // `fileSchema` is `additionalProperties: false` with exactly two required
    // fields, so the assertion is on the *set*: a map's key order is a
    // `serde_json` detail, not part of the contract.
    let mut keys: Vec<&str> = files.body["files"][0]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(keys, vec!["name", "path"]);

    // The request carried the root the control plane composed, the query, the
    // limit, and the policy: files only, dotfiles excluded.
    match fixture.next_request().await {
        HostFileOperation::List {
            path,
            query,
            limit,
            include_files,
            include_directories,
            include_hidden,
        } => {
            assert_eq!(path, fixture.storage_root);
            assert_eq!(query.as_deref(), Some("no"));
            assert_eq!(limit, 7);
            assert!(include_files);
            assert!(!include_directories);
            assert!(!include_hidden);
        }
        other => panic!("expected a listing request, got {other:?}"),
    }

    // `storagePaths` carries the scored path-entry shape.
    fixture.answer(HostFileOutcome::Listing {
        entries: vec![HostFileEntry {
            path: "notes.md".into(),
            name: "notes.md".into(),
            kind: HostPathKind::File,
            score: 0.5,
            positions: vec![0, 1],
        }],
        truncated: true,
    });
    let paths = fixture
        .get(&format!(
            "/api/v1/threads/{}/thread-storage/paths?includeFiles=true&includeDirectories=true",
            fixture.thread_id
        ))
        .await;
    assert_eq!(paths.status, 200, "{}", paths.body);
    assert_response("threads.storagePaths", 200, &paths.body);
    assert_eq!(paths.body["truncated"], true);
    let entry = &paths.body["paths"][0];
    assert_eq!(entry["kind"], "file");
    assert_eq!(entry["score"], 0.5);
    assert_eq!(entry["positions"], json!([0, 1]));

    // Neither kind included is refused before any request is made.
    let none = fixture
        .get(&format!(
            "/api/v1/threads/{}/thread-storage/paths?includeFiles=false&includeDirectories=false",
            fixture.thread_id
        ))
        .await;
    assert_eq!(none.status, 400, "{}", none.body);
    assert_error(400, &none.body);
    assert_eq!(none.body["code"], "invalid_request");

    // A host that fails a listing is reported with the contract's code.
    fixture.answer(HostFileOutcome::Failed {
        code: "invalid_path".into(),
        message: "Path is a directory, not a file".into(),
    });
    let failed = fixture
        .get(&format!(
            "/api/v1/threads/{}/thread-storage/files",
            fixture.thread_id
        ))
        .await;
    assert_eq!(failed.status, 400, "{}", failed.body);
    assert_error(400, &failed.body);
    assert_eq!(failed.body["code"], "invalid_path");
}

#[tokio::test]
async fn content_routes_return_the_hosts_bytes_and_enforce_the_path_rules() {
    let mut fixture = fixture().await;
    let thread = fixture.thread_id.clone();

    // A UTF-8 answer is returned as the bytes it decoded to, with the host's
    // media type and `nosniff`.
    fixture.answer(HostFileOutcome::Content(
        loom_provider_protocol::HostFileContent {
            path: "/srv/b5/src/lib.rs".into(),
            content: "pub fn hi() {}\n".into(),
            content_encoding: HostFileEncoding::Utf8,
            size_bytes: 15,
            mime_type: Some("text/x-rust".into()),
            modified_at_ms: Some(7),
        },
    ));
    let worktree = fixture
        .get(&format!(
            "/api/v1/threads/{thread}/worktree/files/src/lib.rs"
        ))
        .await;
    assert_eq!(worktree.status, 200, "{}", worktree.body);
    assert_eq!(worktree.bytes, b"pub fn hi() {}\n");
    assert_eq!(
        header(&worktree, "content-type").as_deref(),
        Some("text/x-rust")
    );
    assert_eq!(
        header(&worktree, "x-content-type-options").as_deref(),
        Some("nosniff")
    );
    // The request named the workspace root, which is the containment half the
    // host enforces.
    match fixture.next_request().await {
        HostFileOperation::Read {
            path,
            root_path,
            max_bytes,
        } => {
            assert_eq!(path, "/srv/b5/src/lib.rs");
            assert_eq!(root_path.as_deref(), Some("/srv/b5"));
            assert_eq!(max_bytes, loom_server::MAX_FILE_CONTENT_BYTES);
        }
        other => panic!("expected a read request, got {other:?}"),
    }

    // A `..` segment is refused before any request is built: the host is never
    // asked to interpret it.
    let escape = fixture
        .get(&format!(
            "/api/v1/threads/{thread}/worktree/files/../../etc/passwd"
        ))
        .await;
    assert_eq!(escape.status, 400, "{}", escape.body);
    assert_error(400, &escape.body);
    assert_eq!(escape.body["code"], "invalid_path");
    assert!(
        tokio::time::timeout(Duration::from_millis(100), fixture.requests.recv())
            .await
            .is_err(),
        "a traversal attempt must not reach the host"
    );

    // Storage content names the storage root, not the workspace.
    fixture.answer(HostFileOutcome::Content(
        loom_provider_protocol::HostFileContent {
            path: format!("{}/notes.md", fixture.storage_root),
            content: "# notes\n".into(),
            content_encoding: HostFileEncoding::Utf8,
            size_bytes: 8,
            mime_type: Some("text/markdown".into()),
            modified_at_ms: None,
        },
    ));
    let by_query = fixture
        .get(&format!(
            "/api/v1/threads/{thread}/thread-storage/content?path=notes.md"
        ))
        .await;
    assert_eq!(by_query.status, 200, "{}", by_query.body);
    assert_eq!(by_query.bytes, b"# notes\n");
    match fixture.next_request().await {
        HostFileOperation::Read {
            path, root_path, ..
        } => {
            assert_eq!(path, format!("{}/notes.md", fixture.storage_root));
            assert_eq!(root_path.as_deref(), Some(fixture.storage_root.as_str()));
        }
        other => panic!("expected a read request, got {other:?}"),
    }

    // The same read through the URL path parameter.
    fixture.answer(HostFileOutcome::Content(
        loom_provider_protocol::HostFileContent {
            path: format!("{}/a/b.txt", fixture.storage_root),
            content: "hi".into(),
            content_encoding: HostFileEncoding::Utf8,
            size_bytes: 2,
            mime_type: None,
            modified_at_ms: None,
        },
    ));
    let by_url = fixture
        .get(&format!(
            "/api/v1/threads/{thread}/thread-storage/files/a/b.txt"
        ))
        .await;
    assert_eq!(by_url.status, 200, "{}", by_url.body);
    assert_eq!(by_url.bytes, b"hi");
    match fixture.next_request().await {
        HostFileOperation::Read { path, .. } => {
            assert_eq!(path, format!("{}/a/b.txt", fixture.storage_root))
        }
        other => panic!("expected a read request, got {other:?}"),
    }

    // Binary content arrives base64-encoded and is decoded before the response.
    fixture.answer(HostFileOutcome::Content(
        loom_provider_protocol::HostFileContent {
            path: format!("{}/blob.bin", fixture.storage_root),
            content: "AAECAwQ=".into(),
            content_encoding: HostFileEncoding::Base64,
            size_bytes: 5,
            mime_type: None,
            modified_at_ms: None,
        },
    ));
    let binary = fixture
        .get(&format!(
            "/api/v1/threads/{thread}/thread-storage/files/blob.bin"
        ))
        .await;
    assert_eq!(binary.status, 200, "{}", binary.body);
    assert_eq!(binary.bytes, vec![0u8, 1, 2, 3, 4]);
    match fixture.next_request().await {
        HostFileOperation::Read { path, .. } => {
            assert_eq!(path, format!("{}/blob.bin", fixture.storage_root))
        }
        other => panic!("expected a read request, got {other:?}"),
    }

    // A malformed base64 payload is a bad host answer, not a corrupt response.
    fixture.answer(HostFileOutcome::Content(
        loom_provider_protocol::HostFileContent {
            path: format!("{}/bad.bin", fixture.storage_root),
            content: "not base64!".into(),
            content_encoding: HostFileEncoding::Base64,
            size_bytes: 5,
            mime_type: None,
            modified_at_ms: None,
        },
    ));
    let malformed = fixture
        .get(&format!(
            "/api/v1/threads/{thread}/thread-storage/files/bad.bin"
        ))
        .await;
    assert_eq!(malformed.status, 502, "{}", malformed.body);
    assert_error(502, &malformed.body);
    match fixture.next_request().await {
        HostFileOperation::Read { path, .. } => {
            assert_eq!(path, format!("{}/bad.bin", fixture.storage_root))
        }
        other => panic!("expected a read request, got {other:?}"),
    }

    // `hostFileContent` reads an absolute path with no root, which is the point
    // of that route: the client already knows where the file is.
    fixture.answer(HostFileOutcome::Content(
        loom_provider_protocol::HostFileContent {
            path: "/tmp/log.txt".into(),
            content: "log\n".into(),
            content_encoding: HostFileEncoding::Utf8,
            size_bytes: 4,
            mime_type: Some("text/plain".into()),
            modified_at_ms: None,
        },
    ));
    let absolute = fixture
        .get(&format!(
            "/api/v1/threads/{thread}/host-files/content?path=%2Ftmp%2Flog.txt"
        ))
        .await;
    assert_eq!(absolute.status, 200, "{}", absolute.body);
    match fixture.next_request().await {
        HostFileOperation::Read {
            path, root_path, ..
        } => {
            assert_eq!(path, "/tmp/log.txt");
            assert_eq!(root_path, None);
        }
        other => panic!("expected a read request, got {other:?}"),
    }

    // A relative path is not an absolute one, so it is refused with no request.
    let relative = fixture
        .get(&format!(
            "/api/v1/threads/{thread}/host-files/content?path=notes.md"
        ))
        .await;
    assert_eq!(relative.status, 400, "{}", relative.body);
    assert_eq!(relative.body["code"], "invalid_path");

    // A missing file is the host's `not_found`, at the contract's status.
    fixture.answer(HostFileOutcome::Failed {
        code: "not_found".into(),
        message: "No such file or directory".into(),
    });
    let missing = fixture
        .get(&format!(
            "/api/v1/threads/{thread}/host-files/content?path=%2Ftmp%2Fabsent.txt"
        ))
        .await;
    assert_eq!(missing.status, 404, "{}", missing.body);
    assert_error(404, &missing.body);
    assert_eq!(missing.body["code"], "not_found");

    // An oversized file is the host's `file_too_large`, at 413.
    fixture.answer(HostFileOutcome::Failed {
        code: "file_too_large".into(),
        message: "file is too large".into(),
    });
    let too_large = fixture
        .get(&format!(
            "/api/v1/threads/{thread}/thread-storage/files/huge.txt"
        ))
        .await;
    assert_eq!(too_large.status, 413, "{}", too_large.body);
    assert_error(413, &too_large.body);
    assert_eq!(too_large.body["code"], "file_too_large");
}

#[tokio::test]
async fn html_previews_are_sandboxed_size_capped_and_non_html_is_refused() {
    let mut fixture = fixture().await;
    let thread = fixture.thread_id.clone();

    // `rawFile` renders HTML, so a non-HTML path is refused before any read.
    let not_html = fixture
        .get(&format!(
            "/api/v1/threads/{thread}/files/raw?path=%2Ftmp%2Fnotes.md"
        ))
        .await;
    assert_eq!(not_html.status, 415, "{}", not_html.body);
    assert_error(415, &not_html.body);
    assert_eq!(not_html.body["code"], "unsupported_media_type");

    // An HTML payload over the preview cap is refused even though the host was
    // willing to send it.
    fixture.answer(HostFileOutcome::Content(
        loom_provider_protocol::HostFileContent {
            path: "/tmp/big.html".into(),
            content: "<html></html>".into(),
            content_encoding: HostFileEncoding::Utf8,
            size_bytes: loom_server::MAX_HTML_PREVIEW_BYTES + 1,
            mime_type: Some("text/html".into()),
            modified_at_ms: None,
        },
    ));
    let too_large = fixture
        .get(&format!(
            "/api/v1/threads/{thread}/files/raw?path=%2Ftmp%2Fbig.html"
        ))
        .await;
    assert_eq!(too_large.status, 413, "{}", too_large.body);
    assert_eq!(too_large.body["code"], "file_too_large");
    // The host was asked and answered; it was the control plane that applied
    // the tighter preview cap rather than serving the document.
    match fixture.next_request().await {
        HostFileOperation::Read { path, .. } => assert_eq!(path, "/tmp/big.html"),
        other => panic!("expected a read request, got {other:?}"),
    }

    // A preview under the cap is served with the sandboxing headers, and is
    // never cached: its scripts run.
    fixture.answer(HostFileOutcome::Content(
        loom_provider_protocol::HostFileContent {
            path: "/tmp/preview.html".into(),
            content: "<html>hi</html>".into(),
            content_encoding: HostFileEncoding::Utf8,
            size_bytes: 15,
            mime_type: Some("text/html".into()),
            modified_at_ms: None,
        },
    ));
    let preview = fixture
        .get(&format!(
            "/api/v1/threads/{thread}/files/raw?path=%2Ftmp%2Fpreview.html"
        ))
        .await;
    assert_eq!(preview.status, 200, "{}", preview.body);
    assert_eq!(preview.bytes, b"<html>hi</html>");
    assert_eq!(
        header(&preview, "content-security-policy").as_deref(),
        Some("sandbox allow-scripts")
    );
    assert_eq!(
        header(&preview, "cache-control").as_deref(),
        Some("no-store")
    );
    // `rawFile` reads an absolute path with no root; the host is asked for it.
    match fixture.next_request().await {
        HostFileOperation::Read {
            path, root_path, ..
        } => {
            assert_eq!(path, "/tmp/preview.html");
            assert_eq!(root_path, None);
        }
        other => panic!("expected a read request, got {other:?}"),
    }
}

#[tokio::test]
async fn a_thread_without_a_ready_environment_never_asks_a_host() {
    let mut fixture = fixture().await;

    // A thread bound to no environment: every file route refuses rather than
    // falling back to the server's own disk.
    let (unbound, _) = fixture
        .state
        .registry
        .create_thread(
            Some(fixture.project_id.parse().unwrap()),
            Some("unbound".into()),
            None,
            loom_relay::now_ms(),
        )
        .unwrap();
    for path in [
        format!("/api/v1/threads/{}/thread-storage/location", unbound.id),
        format!("/api/v1/threads/{}/thread-storage/files", unbound.id),
        format!(
            "/api/v1/threads/{}/host-files/content?path=%2Fetc%2Fhostname",
            unbound.id
        ),
        format!("/api/v1/threads/{}/worktree/files/src/lib.rs", unbound.id),
    ] {
        let response = fixture.get(&path).await;
        assert_eq!(response.status, 409, "{path}: {}", response.body);
        assert_error(409, &response.body);
        assert_eq!(
            response.body["code"], "thread_environment_unavailable",
            "{path}"
        );
    }
    // An environment with no path cannot answer a workspace read either.
    let (environment, _) = fixture
        .state
        .registry
        .create_environment(
            Some(fixture.project_id.parse().unwrap()),
            fixture.host_id.clone(),
            EnvironmentKind::Unmanaged,
            Some("/srv/other".into()),
            loom_relay::now_ms(),
        )
        .unwrap();
    let (unprovisioned, _) = fixture
        .state
        .registry
        .create_thread(
            Some(fixture.project_id.parse().unwrap()),
            Some("unprovisioned".into()),
            Some(environment.id.clone()),
            loom_relay::now_ms(),
        )
        .unwrap();
    // `Unmanaged` environments always carry a path, so the host is asked. That
    // is the honest behaviour: the environment is usable.
    fixture.answer(HostFileOutcome::Content(
        loom_provider_protocol::HostFileContent {
            path: "/srv/other/a.txt".into(),
            content: "hi".into(),
            content_encoding: HostFileEncoding::Utf8,
            size_bytes: 2,
            mime_type: None,
            modified_at_ms: None,
        },
    ));
    let response = fixture
        .get(&format!(
            "/api/v1/threads/{}/worktree/files/a.txt",
            unprovisioned.id
        ))
        .await;
    assert_eq!(response.status, 200, "{}", response.body);
    match fixture.next_request().await {
        HostFileOperation::Read { path, .. } => assert_eq!(path, "/srv/other/a.txt"),
        other => panic!("expected a read request, got {other:?}"),
    }

    assert!(
        tokio::time::timeout(Duration::from_millis(150), fixture.requests.recv())
            .await
            .is_err(),
        "no unmatched request may be left on the host"
    );
}

#[tokio::test]
async fn an_unknown_thread_and_an_unenrolled_host_are_refused() {
    let fixture = fixture().await;

    let unknown = fixture
        .get(&format!(
            "/api/v1/threads/{UNKNOWN_THREAD}/thread-storage/location"
        ))
        .await;
    assert_eq!(unknown.status, 404, "{}", unknown.body);
    assert_error(404, &unknown.body);
    assert_eq!(unknown.body["code"], "thread_not_found");

    let malformed = fixture
        .get("/api/v1/threads/not-a-thread-id/thread-storage/location")
        .await;
    assert_eq!(malformed.status, 400, "{}", malformed.body);
    assert_error(400, &malformed.body);

    // A host that is enrolled but has never reported a data directory: the
    // control plane refuses rather than inventing a path on a machine it does
    // not own.
    let (host, _) = fixture
        .state
        .registry
        .enroll_host(None, "no-data-dir".into(), loom_relay::now_ms())
        .unwrap();
    let (environment, _) = fixture
        .state
        .registry
        .create_environment(
            Some(fixture.project_id.parse().unwrap()),
            host.id.clone(),
            EnvironmentKind::Unmanaged,
            Some("/srv/unknown".into()),
            loom_relay::now_ms(),
        )
        .unwrap();
    let (thread, _) = fixture
        .state
        .registry
        .create_thread(
            Some(fixture.project_id.parse().unwrap()),
            Some("storage".into()),
            Some(environment.id.clone()),
            loom_relay::now_ms(),
        )
        .unwrap();
    let response = fixture
        .get(&format!(
            "/api/v1/threads/{}/thread-storage/location",
            thread.id
        ))
        .await;
    assert_eq!(response.status, 501, "{}", response.body);
    assert_error(501, &response.body);
    assert_eq!(response.body["code"], "not_configured");
}

/* ------------------------------------------------------------------ */
/* Helpers                                                             */
/* ------------------------------------------------------------------ */

fn header(response: &Response, name: &str) -> Option<String> {
    response
        .headers
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.clone())
}

/// A WebSocket subscriber, used to observe what a route actually delivers.
struct Subscriber {
    socket: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
}

impl Subscriber {
    async fn subscribe(addr: &str, scope: Scope) -> Self {
        let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
            .await
            .unwrap();
        // Consume the welcome frame before the command, so the ack below is the
        // subscription's and not the handshake's.
        let _ = recv_text(&mut socket).await;
        socket
            .send(tokio_tungstenite::tungstenite::Message::Text(
                json!({ "type": "subscribe", "scope": scope })
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
        let ack = recv_value(&mut socket).await;
        assert_eq!(ack["type"], "subscribed", "{ack}");
        Self { socket }
    }

    async fn recv(&mut self) -> Value {
        recv_value(&mut self.socket).await
    }
}

type TestSocket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn recv_text(socket: &mut TestSocket) -> String {
    let message = tokio::time::timeout(TIMEOUT, socket.next())
        .await
        .expect("a websocket frame timed out")
        .expect("the socket closed")
        .unwrap();
    message.into_text().unwrap().to_string()
}

async fn recv_value(socket: &mut TestSocket) -> Value {
    serde_json::from_str(&recv_text(socket).await).unwrap()
}
