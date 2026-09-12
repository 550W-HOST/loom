//! B2 conformance: the fourteen thread-control and auxiliary-view routes.
//!
//! Every assertion here is against `loom-contract`, the same artifact the
//! client is written against, over a real listener — a handler that reshapes a
//! response or invents a field fails the build rather than the client.
//!
//! Each route gets the coverage the acceptance criteria name:
//!
//! * a success case whose body is validated against the route's declared
//!   schema for its status (`validate_response`);
//! * for write routes, the request half: a contract-shaped body is accepted and
//!   one outside the contract is rejected, both as
//!   `validate_request_by_id` samples and against the live server, where the
//!   `validate_contract_request` middleware answers `422`;
//! * the refusal cases the routes have (a stale tab revision, a retry with a
//!   run in flight, compaction) checked as the uniform error body, with the
//!   code drawn from the contract's own list and at a status the contract
//!   declares for it.

use std::future::Future;
use std::time::Duration;

use loom_contract::shared;
use loom_domain::{EnvironmentKind, MessageRole};
use loom_relay::Scope;
use loom_server::http::router;
use loom_server::state::{AppConfig, AppState};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

const TIMEOUT: Duration = Duration::from_secs(5);

// --- harness ----------------------------------------------------------------

/// Starts a server on an ephemeral port and returns its address and state.
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

struct Response {
    status: u16,
    body: Value,
}

/// One HTTP request, parsed far enough to read its status and JSON body.
async fn request(addr: &str, method: &str, path: &str, body: Option<&Value>) -> Response {
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
    tokio::time::timeout(TIMEOUT, stream.read_to_end(&mut raw))
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
    let body =
        serde_json::from_str(body).unwrap_or_else(|error| panic!("bad JSON body: {error}\n{body}"));
    Response { status, body }
}

/// Asserts a response body against the route's schema for its status.
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

/// Asserts the uniform error shape and that the code is one the contract lists.
///
/// Used for the middleware's `422`, which precedes any handler: whether that
/// status is the one the code declares is W-554's existing decision, so this
/// checks the body and the code's membership, not the pair.
#[track_caller]
fn assert_error_shape(body: &Value) {
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

/// Asserts the uniform error shape, a known code, and a status that code
/// declares.
#[track_caller]
fn assert_error(status: u16, body: &Value) {
    assert_error_shape(body);
    let code = body["code"].as_str().unwrap();
    let declared = shared().error_statuses(code);
    assert!(
        declared.contains(&u64::from(status)),
        "code {code:?} is not declared at {status}; the contract declares it at {declared:?}"
    );
}

/// The shared fixture: a project, a connected host, a ready environment and a
/// thread bound to it.
struct Fixture {
    addr: String,
    state: AppState,
    project_id: String,
    host_id: String,
    thread_id: String,
}

async fn fixture() -> Fixture {
    let (addr, state) = spawn_server().await;
    let now = loom_relay::now_ms();
    let (project, _) = state
        .registry
        .create_project("b2".into(), loom_domain::ProjectKind::Standard, None, now)
        .unwrap();
    let (host, _) = state
        .registry
        .enroll_host(None, "b2-host".into(), now)
        .unwrap();
    let (environment, _) = state
        .registry
        .create_environment(
            Some(project.id.clone()),
            host.id.clone(),
            EnvironmentKind::Unmanaged,
            Some("/srv/b2".into()),
            now,
        )
        .unwrap();
    let (thread, _) = state
        .registry
        .create_thread(
            Some(project.id.clone()),
            Some("b2 thread".into()),
            Some(environment.id.clone()),
            now,
        )
        .unwrap();
    Fixture {
        addr,
        state,
        project_id: project.id.to_string(),
        host_id: host.id.to_string(),
        thread_id: thread.id.to_string(),
    }
}

impl Fixture {
    /// A second thread in the same project, with no messages and no run.
    fn new_thread(&self, title: &str) -> String {
        self.state
            .registry
            .create_thread(
                Some(self.project_id.parse().unwrap()),
                Some(title.into()),
                None,
                loom_relay::now_ms(),
            )
            .unwrap()
            .0
            .id
            .to_string()
    }

    fn get<'a>(&'a self, path: &'a str) -> impl Future<Output = Response> + 'a {
        request(&self.addr, "GET", path, None)
    }

    /// Posts a message the way the send route does — through the registry and
    /// into the thread's room — without dispatching a run the read-only routes
    /// under test do not care about.
    fn post(&self, role: MessageRole, content: &str) {
        self.post_into(&self.thread_id, role, content);
    }

    /// The same, into any thread in this fixture's state.
    fn post_into(&self, thread_id: &str, role: MessageRole, content: &str) {
        let events = self
            .state
            .registry
            .post_message(
                &thread_id.parse().unwrap(),
                role,
                content.into(),
                loom_relay::now_ms(),
            )
            .unwrap();
        for event in &events {
            self.state.publish_domain_event(event).unwrap();
        }
    }
}

/// Connects a client and subscribes it to one scope.
struct Subscriber {
    socket: WebSocketStream<MaybeTlsStream<TcpStream>>,
}

impl Subscriber {
    async fn subscribe(addr: &str, scope: Scope) -> Self {
        let (mut socket, _) = connect_async(format!("ws://{addr}/ws")).await.unwrap();
        let welcome = recv(&mut socket).await;
        assert_eq!(welcome["type"], "welcome");
        send(&mut socket, json!({ "type": "subscribe", "scope": scope })).await;
        let ack = recv(&mut socket).await;
        assert_eq!(ack["type"], "subscribed", "unexpected ack: {ack}");
        Self { socket }
    }

    async fn recv(&mut self) -> Value {
        recv(&mut self.socket).await
    }
}

async fn send(socket: &mut WebSocketStream<MaybeTlsStream<TcpStream>>, value: Value) {
    use futures_util::SinkExt;
    socket
        .send(Message::Text(value.to_string().into()))
        .await
        .unwrap();
}

async fn recv(socket: &mut WebSocketStream<MaybeTlsStream<TcpStream>>) -> Value {
    use futures_util::StreamExt;
    let message = tokio::time::timeout(TIMEOUT, socket.next())
        .await
        .expect("timed out waiting for a frame")
        .expect("socket closed")
        .expect("socket error");
    match message {
        Message::Text(text) => serde_json::from_str(text.as_str()).unwrap(),
        other => panic!("expected a text frame, got {other:?}"),
    }
}

// --- request-side conformance ----------------------------------------------

/// The five B2 routes with a JSON request body, each sampled inside and
/// outside the contract.
///
/// This is the blind spot W-554 closed: a response assertion cannot see a
/// request dialect the contract does not declare, so each of these is asserted
/// directly against the contract artifact as well as through the live
/// middleware below. The calls name their route literally — the coverage
/// check (`scripts/check-api-coverage.mjs`) greps for exactly that, so an
/// implemented write route cannot lose its request-side assertion unnoticed.
#[test]
fn write_routes_accept_only_contract_shaped_bodies() {
    let contract = shared();

    let inside = json!({ "title": "renamed", "visibility": "hidden" });
    assert!(
        contract
            .validate_request_by_id("threads.update", &inside)
            .is_empty(),
        "threads.update rejects a contract-shaped body"
    );
    // `title` is `string(min 1) | null`; the empty string is neither.
    let outside = json!({ "title": "" });
    assert!(
        !contract
            .validate_request_by_id("threads.update", &outside)
            .is_empty(),
        "threads.update accepts a body outside the contract"
    );

    let inside = json!({
        "expectedRevision": 0,
        "tabs": [{ "id": "tab-1", "kind": "thread-info" }],
    });
    assert!(
        contract
            .validate_request_by_id("threads.updateTabs", &inside)
            .is_empty(),
        "threads.updateTabs rejects a contract-shaped body"
    );
    // Every tab variant needs its `id`.
    let outside = json!({ "expectedRevision": 0, "tabs": [{ "kind": "thread-info" }] });
    assert!(
        !contract
            .validate_request_by_id("threads.updateTabs", &outside)
            .is_empty(),
        "threads.updateTabs accepts a body outside the contract"
    );

    let inside = json!({
        "file": { "source": "workspace", "path": "src/main.rs", "lineNumber": null },
    });
    assert!(
        contract
            .validate_request_by_id("threads.open", &inside)
            .is_empty(),
        "threads.open rejects a contract-shaped body"
    );
    // `source` is `workspace | thread-storage`.
    let outside = json!({
        "file": { "source": "editor", "path": "src/main.rs", "lineNumber": null },
    });
    assert!(
        !contract
            .validate_request_by_id("threads.open", &outside)
            .is_empty(),
        "threads.open accepts a body outside the contract"
    );

    let inside = json!({ "reason": "Retry" });
    assert!(
        contract
            .validate_request_by_id("threads.retry", &inside)
            .is_empty(),
        "threads.retry rejects a contract-shaped body"
    );
    // `reason` is `string(min 1, max 200)`. (`turnRequestId`'s `pattern` is
    // checked by the handler, because the contract's validator deliberately
    // does not enforce patterns.)
    let outside = json!({ "reason": "" });
    assert!(
        !contract
            .validate_request_by_id("threads.retry", &outside)
            .is_empty(),
        "threads.retry accepts a body outside the contract"
    );

    let inside = json!({
        "input": [{ "type": "text", "text": "try again" }],
        "operationId": "op-1",
    });
    assert!(
        contract
            .validate_request_by_id("threads.editMessage", &inside)
            .is_empty(),
        "threads.editMessage rejects a contract-shaped body"
    );
    // `input` has at least one item.
    let outside = json!({ "input": [], "operationId": "op-1" });
    assert!(
        !contract
            .validate_request_by_id("threads.editMessage", &outside)
            .is_empty(),
        "threads.editMessage accepts a body outside the contract"
    );
}

#[tokio::test]
async fn the_live_server_rejects_bodies_outside_the_contract() {
    let fixture = fixture().await;
    let thread_id = &fixture.thread_id;
    let cases: [(&str, &str, Value); 4] = [
        ("threads.update", "PATCH", json!({ "title": "" })),
        (
            "threads.updateTabs",
            "PUT",
            json!({ "expectedRevision": 0 }),
        ),
        (
            "threads.open",
            "POST",
            json!({ "file": { "source": "workspace", "path": "", "lineNumber": null } }),
        ),
        (
            "threads.editMessage",
            "POST",
            json!({ "input": [], "operationId": "op-1" }),
        ),
    ];
    for (route_id, method, body) in cases {
        let path = match route_id {
            "threads.update" => format!("/api/v1/threads/{thread_id}"),
            "threads.updateTabs" => format!("/api/v1/threads/{thread_id}/tabs"),
            "threads.open" => format!("/api/v1/threads/{thread_id}/open"),
            _ => format!("/api/v1/threads/{thread_id}/edit-message"),
        };
        let response = request(&fixture.addr, method, &path, Some(&body)).await;
        assert_eq!(
            response.status, 422,
            "{route_id} should refuse a body it cannot validate: {}",
            response.body
        );
        assert_error_shape(&response.body);
        assert_eq!(response.body["code"], "invalid_request", "{route_id}");
    }

    // `threads.retry`'s `turnRequestId` is a `pattern`, and the contract's
    // validator does not enforce patterns, so this one is the handler's own
    // check — an id it cannot echo back must be refused rather than repeated.
    let response = request(
        &fixture.addr,
        "POST",
        &format!("/api/v1/threads/{thread_id}/retry"),
        Some(&json!({ "turnRequestId": "req-1" })),
    )
    .await;
    assert_eq!(response.status, 400, "{}", response.body);
    assert_error(400, &response.body);
    assert_eq!(response.body["code"], "invalid_request");
    fixture.state.shutdown();
}

// --- read routes ------------------------------------------------------------

#[tokio::test]
async fn projects_default_execution_options_report_the_configured_provider() {
    let fixture = fixture().await;
    let response = fixture
        .get(&format!(
            "/api/v1/projects/{}/default-execution-options",
            fixture.project_id
        ))
        .await;
    assert_eq!(response.status, 200, "{}", response.body);
    assert_response("projects.defaultExecutionOptions", 200, &response.body);
    assert!(response.body["providerId"].is_string(), "{}", response.body);
    assert!(response.body["model"].is_string(), "{}", response.body);
    fixture.state.shutdown();
}

#[tokio::test]
async fn child_summary_counts_threads_that_name_a_parent() {
    let fixture = fixture().await;
    let child_id = fixture.new_thread("child");

    let before = fixture
        .get(&format!(
            "/api/v1/threads/{}/child-summary",
            fixture.thread_id
        ))
        .await;
    assert_eq!(before.status, 200, "{}", before.body);
    assert_response("threads.childSummary", 200, &before.body);
    assert_eq!(before.body["nonDeletedChildCount"], 0);

    // `threads.update` is what files a thread under a parent, so the count is
    // proven through the write route that produces it.
    let updated = request(
        &fixture.addr,
        "PATCH",
        &format!("/api/v1/threads/{child_id}"),
        Some(&json!({ "parentThreadId": fixture.thread_id })),
    )
    .await;
    assert_eq!(updated.status, 200, "{}", updated.body);
    assert_response("threads.update", 200, &updated.body);
    assert_eq!(updated.body["parentThreadId"], fixture.thread_id);

    let after = fixture
        .get(&format!(
            "/api/v1/threads/{}/child-summary",
            fixture.thread_id
        ))
        .await;
    assert_eq!(after.body["nonDeletedChildCount"], 1, "{}", after.body);
    fixture.state.shutdown();
}

#[tokio::test]
async fn conversation_outline_lists_user_and_assistant_turns() {
    let fixture = fixture().await;
    fixture.post(MessageRole::User, "  what does\n the outage look like?  ");
    fixture.post(MessageRole::Assistant, "it is a 500 on the write path");

    let response = fixture
        .get(&format!(
            "/api/v1/threads/{}/conversation-outline",
            fixture.thread_id
        ))
        .await;
    assert_eq!(response.status, 200, "{}", response.body);
    assert_response("threads.conversationOutline", 200, &response.body);
    let items = response.body["items"].as_array().unwrap();
    assert_eq!(items.len(), 2, "{}", response.body);
    assert_eq!(items[0]["role"], "user");
    // Whitespace is collapsed so the preview is one line.
    assert_eq!(items[0]["preview"], "what does the outage look like?");
    assert_eq!(items[1]["role"], "assistant");
    assert!(items[0]["attachmentSummary"].is_null());
    assert!(
        response.body["maxSeq"].as_u64().unwrap() >= 2,
        "{}",
        response.body
    );
    fixture.state.shutdown();
}

#[tokio::test]
async fn prompt_history_returns_user_prompts_newest_first() {
    let fixture = fixture().await;
    for prompt in ["first prompt", "second prompt"] {
        fixture.post(MessageRole::User, prompt);
    }
    fixture.post(MessageRole::Assistant, "an answer is not a prompt");

    let response = fixture
        .get(&format!(
            "/api/v1/threads/{}/prompt-history",
            fixture.thread_id
        ))
        .await;
    assert_eq!(response.status, 200, "{}", response.body);
    assert_response("threads.promptHistory", 200, &response.body);
    let prompts = response.body.as_array().expect("a bare array");
    assert_eq!(prompts.len(), 2, "{}", response.body);
    assert_eq!(prompts[0]["input"][0]["text"], "second prompt");
    assert_eq!(prompts[0]["input"][0]["type"], "text");
    assert_eq!(prompts[1]["input"][0]["text"], "first prompt");

    let limited = fixture
        .get(&format!(
            "/api/v1/threads/{}/prompt-history?limit=1",
            fixture.thread_id
        ))
        .await;
    assert_eq!(limited.status, 200, "{}", limited.body);
    assert_response("threads.promptHistory", 200, &limited.body);
    assert_eq!(limited.body.as_array().unwrap().len(), 1);
    fixture.state.shutdown();
}

#[tokio::test]
async fn default_execution_options_are_null_until_a_client_records_them() {
    let fixture = fixture().await;
    let path = format!(
        "/api/v1/threads/{}/default-execution-options",
        fixture.thread_id
    );

    let unset = fixture.get(&path).await;
    assert_eq!(unset.status, 200, "{}", unset.body);
    assert_response("threads.defaultExecutionOptions", 200, &unset.body);
    assert!(unset.body.is_null(), "{}", unset.body);

    let updated = request(
        &fixture.addr,
        "PATCH",
        &format!("/api/v1/threads/{}", fixture.thread_id),
        Some(&json!({ "model": "pi", "reasoningLevel": "high" })),
    )
    .await;
    assert_eq!(updated.status, 200, "{}", updated.body);
    assert_response("threads.update", 200, &updated.body);

    let set = fixture.get(&path).await;
    assert_eq!(set.status, 200, "{}", set.body);
    assert_response("threads.defaultExecutionOptions", 200, &set.body);
    assert_eq!(set.body["model"], "pi");
    assert_eq!(set.body["reasoningLevel"], "high");
    assert_eq!(set.body["source"], "client/thread/start");
    fixture.state.shutdown();
}

#[tokio::test]
async fn running_lists_the_threads_with_a_run_in_flight() {
    let fixture = fixture().await;
    let empty = fixture.get("/api/v1/threads/running").await;
    assert_eq!(empty.status, 200, "{}", empty.body);
    assert_response("threads.running", 200, &empty.body);
    assert_eq!(empty.body.as_array().unwrap().len(), 0);

    // A user turn is what starts a run: the thread moves to `working` and the
    // dispatch is recorded against the environment's host.
    let sent = request(
        &fixture.addr,
        "POST",
        &format!("/api/v1/threads/{}/send", fixture.thread_id),
        Some(&json!({
            "input": [{ "type": "text", "text": "take a look at the outage" }],
            "mode": "auto",
        })),
    )
    .await;
    assert_eq!(sent.status, 200, "{}", sent.body);

    let running = fixture.get("/api/v1/threads/running").await;
    assert_eq!(running.status, 200, "{}", running.body);
    assert_response("threads.running", 200, &running.body);
    let rows = running.body.as_array().unwrap();
    assert_eq!(rows.len(), 1, "{}", running.body);
    assert_eq!(rows[0]["id"], fixture.thread_id);
    assert_eq!(rows[0]["hostId"], fixture.host_id);
    fixture.state.shutdown();
}

#[tokio::test]
async fn search_groups_matches_by_archive_state() {
    let fixture = fixture().await;
    fixture.post(
        MessageRole::User,
        "the checkout endpoint returns a bad status",
    );
    // A decoy thread that does not match, so a result list of one proves the
    // search filtered rather than returned everything.
    fixture.new_thread("unrelated work");

    let response = fixture.get("/api/v1/threads/search?query=checkout").await;
    assert_eq!(response.status, 200, "{}", response.body);
    assert_response("threads.search", 200, &response.body);
    let active = &response.body["active"];
    assert_eq!(active["total"], 1, "{}", response.body);
    let result = &active["results"][0];
    assert_eq!(result["thread"]["id"], fixture.thread_id);
    let matches = result["matches"].as_array().unwrap();
    assert_eq!(matches.len(), 1, "{}", result);
    assert_eq!(matches[0]["sourceKind"], "user_message");
    assert_eq!(
        matches[0]["text"],
        "the checkout endpoint returns a bad status"
    );
    assert_eq!(
        matches[0]["highlightRanges"][0],
        json!({ "start": 4, "end": 12 })
    );
    assert!(matches[0]["sourceSeq"].is_number());
    assert_eq!(response.body["archived"]["total"], 0);

    // The title matches too, and a group limit is honoured with a total that
    // still counts every matching thread.
    let titled = fixture
        .get("/api/v1/threads/search?query=b2%20thread")
        .await;
    assert_eq!(titled.status, 200, "{}", titled.body);
    assert_response("threads.search", 200, &titled.body);
    assert_eq!(
        titled.body["active"]["results"][0]["matches"][0]["sourceKind"],
        "title"
    );

    // A one-character query is outside the contract, and the server says so in
    // the uniform error shape with a code the contract lists.
    let short_query = fixture.get("/api/v1/threads/search?query=e").await;
    assert_eq!(short_query.status, 400, "{}", short_query.body);
    assert_error(400, &short_query.body);
    assert_eq!(short_query.body["code"], "invalid_request");

    // A limit bounds the rows while `total` keeps counting every matching
    // thread: two threads match "checkout", the group returns one.
    let second = fixture.new_thread("another checkout thread");
    fixture.post_into(&second, MessageRole::User, "the checkout is back");
    let limited = fixture
        .get("/api/v1/threads/search?query=checkout&limitPerGroup=1")
        .await;
    assert_eq!(limited.status, 200, "{}", limited.body);
    assert_response("threads.search", 200, &limited.body);
    assert_eq!(limited.body["active"]["total"], 2, "{}", limited.body);
    assert_eq!(
        limited.body["active"]["results"].as_array().unwrap().len(),
        1,
        "{}",
        limited.body
    );

    let none = fixture.get("/api/v1/threads/search?query=zzzzz").await;
    assert_eq!(none.status, 200, "{}", none.body);
    assert_response("threads.search", 200, &none.body);
    assert_eq!(none.body["active"]["total"], 0);
    assert_eq!(none.body["archived"]["total"], 0);
    assert!(none.body["active"]["results"]
        .as_array()
        .unwrap()
        .is_empty());

    fixture.state.shutdown();
}

// --- control routes ---------------------------------------------------------

#[tokio::test]
async fn thread_update_applies_fields_and_answers_the_thread() {
    let fixture = fixture().await;
    let response = request(
        &fixture.addr,
        "PATCH",
        &format!("/api/v1/threads/{}", fixture.thread_id),
        Some(&json!({
            "title": "  renamed  ",
            "sectionId": "sec-1",
            "visibility": "hidden",
            "model": "pi",
            "reasoningLevel": "medium",
        })),
    )
    .await;
    assert_eq!(response.status, 200, "{}", response.body);
    assert_response("threads.update", 200, &response.body);
    assert_eq!(response.body["id"], fixture.thread_id);
    assert_eq!(response.body["title"], "renamed");
    assert_eq!(response.body["sectionId"], "sec-1");
    assert_eq!(response.body["visibility"], "hidden");

    // The change is in the entity view the list routes read, not only in the
    // response.
    let listed = fixture.get("/api/v1/threads").await;
    let row = listed
        .body
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == fixture.thread_id.as_str())
        .expect("the thread is listed");
    assert_eq!(row["title"], "renamed");
    assert_eq!(row["visibility"], "hidden");
    assert_eq!(row["sectionId"], "sec-1");
    fixture.state.shutdown();
}

#[tokio::test]
async fn tabs_round_trip_under_a_revision() {
    let fixture = fixture().await;
    let path = format!("/api/v1/threads/{}/tabs", fixture.thread_id);

    let initial = fixture.get(&path).await;
    assert_eq!(initial.status, 200, "{}", initial.body);
    assert_response("threads.tabs", 200, &initial.body);
    assert_eq!(initial.body["revision"], 0);
    assert!(initial.body["tabs"].as_array().unwrap().is_empty());

    let written = request(
        &fixture.addr,
        "PUT",
        &path,
        Some(&json!({
            "expectedRevision": 0,
            "tabs": [
                { "id": "tab-1", "kind": "thread-info" },
                { "id": "tab-2", "kind": "git-diff" },
            ],
        })),
    )
    .await;
    assert_eq!(written.status, 200, "{}", written.body);
    assert_response("threads.updateTabs", 200, &written.body);
    assert_eq!(written.body["revision"], 1);
    assert_eq!(written.body["tabs"].as_array().unwrap().len(), 2);

    let read_back = fixture.get(&path).await;
    assert_eq!(read_back.status, 200, "{}", read_back.body);
    assert_eq!(read_back.body, written.body, "the read-back differs");

    // The same revision again is a lost update, not a second write.
    let stale = request(
        &fixture.addr,
        "PUT",
        &path,
        Some(&json!({ "expectedRevision": 0, "tabs": [] })),
    )
    .await;
    assert_eq!(stale.status, 409, "{}", stale.body);
    assert_response("threads.updateTabs", 409, &stale.body);
    assert_error(409, &stale.body);
    assert_eq!(stale.body["code"], "thread_tabs_conflict");

    // The refused write did not move the tabs.
    let unchanged = fixture.get(&path).await;
    assert_eq!(unchanged.body, written.body);
    fixture.state.shutdown();
}

#[tokio::test]
async fn open_delivers_to_the_threads_subscribers() {
    let fixture = fixture().await;
    let path = format!("/api/v1/threads/{}/open", fixture.thread_id);

    let unwatched = request(
        &fixture.addr,
        "POST",
        &path,
        Some(&json!({
            "file": { "source": "workspace", "path": "src/lib.rs", "lineNumber": 7 },
            "split": "right",
        })),
    )
    .await;
    assert_eq!(unwatched.status, 200, "{}", unwatched.body);
    assert_response("threads.open", 200, &unwatched.body);
    assert_eq!(unwatched.body["delivered"], 0, "{}", unwatched.body);

    // With a client watching the thread, the open request is both counted and
    // actually delivered to it.
    let mut subscriber =
        Subscriber::subscribe(&fixture.addr, Scope::Thread(fixture.thread_id.clone())).await;
    let watched = request(
        &fixture.addr,
        "POST",
        &path,
        Some(&json!({
            "file": { "source": "thread-storage", "path": "notes.md", "lineNumber": null },
            "split": "down",
        })),
    )
    .await;
    assert_eq!(watched.status, 200, "{}", watched.body);
    assert_response("threads.open", 200, &watched.body);
    assert_eq!(watched.body["delivered"], 1, "{}", watched.body);

    // `delivered: 1` is a count of subscribers; this is the frame they get.
    let frame = subscriber.recv().await;
    assert_eq!(frame["type"], "event");
    let payload: Value = serde_json::from_str(frame["payload"].as_str().unwrap()).unwrap();
    assert_eq!(payload["type"], "thread_open_requested");
    assert_eq!(payload["threadId"], fixture.thread_id);
    assert_eq!(payload["split"], "down");
    fixture.state.shutdown();
}

#[tokio::test]
async fn compact_and_edit_message_refuse_instead_of_pretending() {
    let fixture = fixture().await;

    let compact = request(
        &fixture.addr,
        "POST",
        &format!("/api/v1/threads/{}/compact", fixture.thread_id),
        None,
    )
    .await;
    assert_eq!(compact.status, 501, "{}", compact.body);
    assert_error(501, &compact.body);
    assert_eq!(compact.body["code"], "not_configured");

    let edit = request(
        &fixture.addr,
        "POST",
        &format!("/api/v1/threads/{}/edit-message", fixture.thread_id),
        Some(&json!({
            "input": [{ "type": "text", "text": "try again" }],
            "operationId": "op-1",
        })),
    )
    .await;
    assert_eq!(edit.status, 501, "{}", edit.body);
    assert_error(501, &edit.body);
    assert_eq!(edit.body["code"], "not_configured");
    fixture.state.shutdown();
}

#[tokio::test]
async fn retry_redispatches_the_last_prompt_and_queues_while_busy() {
    let fixture = fixture().await;
    let path = format!("/api/v1/threads/{}/retry", fixture.thread_id);

    // No turn yet: nothing to retry.
    let empty = request(&fixture.addr, "POST", &path, Some(&json!({}))).await;
    assert_eq!(empty.status, 409, "{}", empty.body);
    assert_error(409, &empty.body);
    assert_eq!(empty.body["code"], "no_failed_turn");

    let sent = request(
        &fixture.addr,
        "POST",
        &format!("/api/v1/threads/{}/send", fixture.thread_id),
        Some(&json!({
            "input": [{ "type": "text", "text": "check the deploy" }],
            "mode": "auto",
        })),
    )
    .await;
    assert_eq!(sent.status, 200, "{}", sent.body);

    // Reap the run the way the server's own reconciler would, then retry: the
    // last user prompt is dispatched again through the existing lifecycle.
    let thread_id = fixture.thread_id.parse().unwrap();
    fixture.state.stop_thread(&thread_id);
    let retried = request(
        &fixture.addr,
        "POST",
        &path,
        Some(&json!({ "reason": "the first attempt timed out" })),
    )
    .await;
    assert_eq!(retried.status, 200, "{}", retried.body);
    assert_response("threads.retry", 200, &retried.body);
    assert_eq!(retried.body["ok"], true);
    assert_eq!(retried.body["delivery"], "sent");
    assert_eq!(retried.body["attempt"], 1);
    let turn_request_id = retried.body["turnRequestId"].as_str().unwrap();
    assert!(
        turn_request_id.starts_with("creq_") && turn_request_id.len() == 15,
        "retry answered a turn request id outside bb's shape: {turn_request_id}"
    );

    // And the retry is running: the run table has it under the same thread.
    let running = fixture.get("/api/v1/threads/running").await;
    let rows = running.body.as_array().unwrap();
    assert_eq!(rows.len(), 1, "{}", running.body);
    assert_eq!(rows[0]["id"], fixture.thread_id);

    // B3: a retry of a thread with a run in flight is no longer a `409`. It is
    // the contract's second response branch — a durable queued message with a
    // retry payload — because running it now would be a second concurrent turn
    // and answering `sent` would claim a turn that did not start.
    let busy = request(
        &fixture.addr,
        "POST",
        &path,
        Some(&json!({ "reason": "after the current turn" })),
    )
    .await;
    assert_eq!(busy.status, 200, "{}", busy.body);
    assert_response("threads.retry", 200, &busy.body);
    assert_eq!(busy.body["delivery"], "queued");
    assert_eq!(busy.body["attempt"], 2);
    assert_eq!(busy.body["waitingOn"]["kind"], "thread-busy");
    let queued_message_id = busy.body["queuedMessageId"].as_str().unwrap().to_owned();
    assert!(queued_message_id.starts_with("qmsg_"), "{}", busy.body);

    // The row is a real queue entry with the retry payload, not a placeholder.
    let queued = fixture
        .get(&format!(
            "/api/v1/threads/{}/queued-messages",
            fixture.thread_id
        ))
        .await;
    assert_response("threads.queuedMessages", 200, &queued.body);
    let rows = queued.body.as_array().unwrap();
    assert_eq!(rows.len(), 1, "{}", queued.body);
    assert_eq!(rows[0]["id"], queued_message_id.as_str());
    assert_eq!(rows[0]["payload"]["kind"], "retry");
    assert_eq!(rows[0]["payload"]["attempt"], 2);

    // A scheduled retry is the same queued branch, with the time reason.
    let scheduled = request(
        &fixture.addr,
        "POST",
        &path,
        Some(&json!({ "sendAt": loom_relay::now_ms() + 60_000 })),
    )
    .await;
    assert_eq!(scheduled.status, 200, "{}", scheduled.body);
    assert_response("threads.retry", 200, &scheduled.body);
    assert_eq!(scheduled.body["delivery"], "queued");
    assert_eq!(scheduled.body["waitingOn"]["kind"], "time");
    fixture.state.shutdown();
}

#[tokio::test]
async fn stop_terminates_the_run_and_returns_the_thread_to_idle() {
    let fixture = fixture().await;
    let sent = request(
        &fixture.addr,
        "POST",
        &format!("/api/v1/threads/{}/send", fixture.thread_id),
        Some(&json!({
            "input": [{ "type": "text", "text": "start a long build" }],
            "mode": "auto",
        })),
    )
    .await;
    assert_eq!(sent.status, 200, "{}", sent.body);

    let stopped = request(
        &fixture.addr,
        "POST",
        &format!("/api/v1/threads/{}/stop", fixture.thread_id),
        None,
    )
    .await;
    assert_eq!(stopped.status, 200, "{}", stopped.body);
    assert_response("threads.stop", 200, &stopped.body);
    assert_eq!(stopped.body["ok"], true);

    // The cancellation went through the run lifecycle: the run is gone and the
    // thread is back to `idle`.
    let running = fixture.get("/api/v1/threads/running").await;
    assert!(
        running.body.as_array().unwrap().is_empty(),
        "{}",
        running.body
    );
    let thread = fixture
        .get(&format!("/api/v1/threads/{}", fixture.thread_id))
        .await;
    assert_eq!(thread.body["status"], "idle", "{}", thread.body);

    // Stopping a thread that is not running is the state the caller asked for.
    let again = request(
        &fixture.addr,
        "POST",
        &format!("/api/v1/threads/{}/stop", fixture.thread_id),
        None,
    )
    .await;
    assert_eq!(again.status, 200, "{}", again.body);
    assert_response("threads.stop", 200, &again.body);
    fixture.state.shutdown();
}

#[tokio::test]
async fn every_route_refuses_an_unknown_thread() {
    let fixture = fixture().await;
    let unknown = "thr_01M27Y6Q0J8V4W2C7K5N3P1R9Z";
    let cases: [(&str, &str, &str, Option<Value>); 9] = [
        ("threads.childSummary", "GET", "/child-summary", None),
        ("threads.compact", "POST", "/compact", None),
        (
            "threads.conversationOutline",
            "GET",
            "/conversation-outline",
            None,
        ),
        (
            "threads.defaultExecutionOptions",
            "GET",
            "/default-execution-options",
            None,
        ),
        (
            "threads.editMessage",
            "POST",
            "/edit-message",
            Some(json!({ "input": [{ "type": "text", "text": "x" }], "operationId": "op" })),
        ),
        (
            "threads.open",
            "POST",
            "/open",
            Some(json!({ "file": null })),
        ),
        ("threads.promptHistory", "GET", "/prompt-history", None),
        ("threads.retry", "POST", "/retry", Some(json!({}))),
        ("threads.stop", "POST", "/stop", None),
    ];
    for (route_id, method, suffix, body) in cases {
        let path = format!("/api/v1/threads/{unknown}{suffix}");
        let response = request(&fixture.addr, method, &path, body.as_ref()).await;
        assert_eq!(
            response.status, 404,
            "{route_id} should answer 404 for an unknown thread: {}",
            response.body
        );
        assert_error(404, &response.body);
        assert_eq!(response.body["code"], "not_found", "{route_id}");
    }

    let update = request(
        &fixture.addr,
        "PATCH",
        &format!("/api/v1/threads/{unknown}"),
        Some(&json!({ "title": "nope" })),
    )
    .await;
    assert_eq!(update.status, 404, "{}", update.body);
    assert_error(404, &update.body);

    let tabs = request(
        &fixture.addr,
        "PUT",
        &format!("/api/v1/threads/{unknown}/tabs"),
        Some(&json!({ "expectedRevision": 0, "tabs": [] })),
    )
    .await;
    assert_eq!(tabs.status, 404, "{}", tabs.body);
    assert_error(404, &tabs.body);
    fixture.state.shutdown();
}
