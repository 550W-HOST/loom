//! B3 conformance: interactions, plan control and the queued-message surface.
//!
//! Fourteen routes, and every assertion is against `loom-contract` — the same
//! artifact bb's client is written against — over a real listener. A handler
//! that reshapes a request or a response fails the build rather than the client.
//!
//! The coverage each route gets, in the terms the acceptance criteria use:
//!
//! * a success case whose body is validated against the route's declared schema
//!   for its status (`assert_response`);
//! * for write routes, the request half: a contract-shaped body is accepted and
//!   one outside the contract is rejected, both as `validate_request_by_id`
//!   samples and against the live server, where the `validate_contract_request`
//!   middleware answers `422`;
//! * the refusals the routes have — steer, a mismatch between an interaction's
//!   kind and its resolution, a settled interaction — as the uniform error body
//!   with a code drawn from the contract's own list at a status it declares;
//! * the two new domains' state machines as plain unit-level assertions on the
//!   live entities, plus the persistence round-trip that proves a restart does
//!   not lose a queued message or a pending interaction.

use std::future::Future;
use std::time::Duration;

use loom_contract::shared;
use loom_domain::{
    EnvironmentKind, InteractionKind, InteractionOrigin, InteractionPayload, MessageRole,
    Resolution,
};
use loom_server::http::router;
use loom_server::state::{AppConfig, AppState};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const TIMEOUT: Duration = Duration::from_secs(10);

/// A thread id that is well-formed and unknown, for the 404 cases.
const UNKNOWN_THREAD: &str = "thr_01M27Y6Q0J8V4W2C7K5N3P1R9Z";
/// An interaction id that is well-formed and unknown.
const UNKNOWN_INTERACTION: &str = "intr_01M27Y6Q0J8V4W2C7K5N3P1R9Z";
/// A queued-message id that is well-formed and unknown.
const UNKNOWN_QUEUED_MESSAGE: &str = "qmsg_01M27Y6Q0J8V4W2C7K5N3P1R9Z";

// --- harness ----------------------------------------------------------------

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

/// A project, a connected host, a ready environment and an idle thread.
struct Fixture {
    addr: String,
    state: AppState,
    thread_id: String,
}

async fn fixture() -> Fixture {
    let (addr, state) = spawn_server().await;
    let now = loom_relay::now_ms();
    let (project, _) = state
        .registry
        .create_project("b3".into(), loom_domain::ProjectKind::Standard, None, now)
        .unwrap();
    let (host, _) = state
        .registry
        .enroll_host(None, "b3-host".into(), now)
        .unwrap();
    let (environment, _) = state
        .registry
        .create_environment(
            Some(project.id.clone()),
            host.id.clone(),
            EnvironmentKind::Unmanaged,
            Some("/srv/b3".into()),
            now,
        )
        .unwrap();
    let (thread, _) = state
        .registry
        .create_thread(
            Some(project.id.clone()),
            Some("b3 thread".into()),
            Some(environment.id.clone()),
            now,
        )
        .unwrap();
    Fixture {
        addr,
        state,
        thread_id: thread.id.to_string(),
    }
}

impl Fixture {
    fn get<'a>(&'a self, path: &'a str) -> impl Future<Output = Response> + 'a {
        request(&self.addr, "GET", path, None)
    }

    fn post(&self, path: &str, body: Value) -> impl Future<Output = Response> + '_ {
        let path = path.to_owned();
        let addr = self.addr.clone();
        async move { request(&addr, "POST", &path, Some(&body)).await }
    }

    fn thread_id(&self) -> loom_domain::ThreadId {
        self.thread_id.parse().unwrap()
    }

    /// Records an interaction directly, the way a daemon's report would.
    ///
    /// The payload is the smallest body the contract declares for each kind, so
    /// every response built from it is valid without the route reshaping
    /// anything: an approval over a plan, a one-question prompt, and an opaque
    /// body whose title/body pair the contract defines for exactly this case.
    fn record_interaction(&self, kind: InteractionKind) -> loom_domain::Interaction {
        let body = match kind {
            InteractionKind::Approval => json!({
                "kind": "approval",
                "subject": {
                    "kind": "plan",
                    "itemId": "item-approval",
                    "plan": "run the test suite",
                    "planFilePath": null,
                },
                "reason": null,
                "availableDecisions": ["allow_once", "deny"],
            }),
            InteractionKind::UserQuestion => json!({
                "kind": "user_question",
                "questions": [{
                    "id": "q1",
                    "prompt": "Which branch?",
                    "multiSelect": false,
                    "allowFreeText": false,
                    "options": [{ "value": "main", "label": "main" }],
                }],
            }),
            InteractionKind::Generic => json!({
                "kind": "generic",
                "title": "The plugin needs input",
                "data": null,
            }),
            InteractionKind::Plugin => json!({
                "kind": "plugin",
                "title": "The plugin needs input",
                "data": null,
            }),
        };
        let origin = match kind {
            InteractionKind::Plugin => InteractionOrigin::Plugin {
                plugin_id: "example".into(),
                renderer_id: "example-form".into(),
            },
            _ => InteractionOrigin::Provider {
                provider_id: "pi".into(),
                provider_request_id: format!("req-{}", kind.as_str()),
            },
        };
        let provider_request_id = match &origin {
            InteractionOrigin::Provider {
                provider_request_id,
                ..
            } => Some(provider_request_id.clone()),
            InteractionOrigin::Plugin { .. } => None,
        };
        self.state
            .record_interaction(
                &self.thread_id(),
                "run_01M27Y6Q0J8V4W2C7K5N3P1R9Z",
                kind,
                origin,
                InteractionPayload::new(kind, body),
                provider_request_id.as_deref(),
                None,
                loom_relay::now_ms(),
            )
            .unwrap()
    }
}

// --- request-side conformance ----------------------------------------------

/// The four B3 write routes with a JSON request body, each sampled inside and
/// outside the contract.
///
/// The check is the W-554 guard: a response assertion cannot see a request
/// dialect the contract does not declare, so each of these is asserted against
/// the contract artifact directly as well as through the live middleware
/// below. The calls name their route literally, because
/// `scripts/check-api-coverage.mjs` greps for exactly that and would otherwise
/// refuse to count the route.
#[test]
fn write_routes_accept_only_contract_shaped_bodies() {
    let contract = shared();

    let inside = json!({ "input": [{ "type": "text", "text": "later" }] });
    assert!(
        contract
            .validate_request_by_id("threads.createQueuedMessage", &inside)
            .is_empty(),
        "threads.createQueuedMessage rejects a contract-shaped body"
    );
    // `input` needs at least one block.
    let outside = json!({ "input": [] });
    assert!(
        !contract
            .validate_request_by_id("threads.createQueuedMessage", &outside)
            .is_empty(),
        "threads.createQueuedMessage accepts a body outside the contract"
    );

    let inside = json!({ "mode": "auto" });
    assert!(
        contract
            .validate_request_by_id("threads.sendQueuedMessage", &inside)
            .is_empty(),
        "threads.sendQueuedMessage rejects a contract-shaped body"
    );
    // `mode` is `auto | steer`.
    let outside = json!({ "mode": "now" });
    assert!(
        !contract
            .validate_request_by_id("threads.sendQueuedMessage", &outside)
            .is_empty(),
        "threads.sendQueuedMessage accepts a body outside the contract"
    );

    let inside = json!({ "value": "yes" });
    assert!(
        contract
            .validate_request_by_id("threads.respondToInteraction", &inside)
            .is_empty(),
        "threads.respondToInteraction rejects a contract-shaped body"
    );
    // `value` is required.
    let outside = json!({});
    assert!(
        !contract
            .validate_request_by_id("threads.respondToInteraction", &outside)
            .is_empty(),
        "threads.respondToInteraction accepts a body outside the contract"
    );

    let inside = json!({
        "kind": "user_answer",
        "answers": { "q1": { "selected": ["a"] } },
    });
    assert!(
        contract
            .validate_request_by_id("threads.resolveInteraction", &inside)
            .is_empty(),
        "threads.resolveInteraction rejects a contract-shaped body"
    );
    // `answers` entries need a `selected` array.
    let outside = json!({ "kind": "user_answer", "answers": { "q1": {} } });
    assert!(
        !contract
            .validate_request_by_id("threads.resolveInteraction", &outside)
            .is_empty(),
        "threads.resolveInteraction accepts a body outside the contract"
    );
}

#[tokio::test]
async fn the_live_server_rejects_bodies_outside_the_contract() {
    let fixture = fixture().await;
    let thread_id = &fixture.thread_id;
    let cases: [(&str, Value); 4] = [
        ("threads.createQueuedMessage", json!({ "input": [] })),
        ("threads.sendQueuedMessage", json!({ "mode": "later" })),
        ("threads.respondToInteraction", json!({})),
        (
            "threads.resolveInteraction",
            json!({ "kind": "user_answer" }),
        ),
    ];
    for (route_id, body) in cases {
        let path = match route_id {
            "threads.createQueuedMessage" => format!("/api/v1/threads/{thread_id}/queued-messages"),
            "threads.sendQueuedMessage" => {
                format!("/api/v1/threads/{thread_id}/queued-messages/{UNKNOWN_QUEUED_MESSAGE}/send")
            }
            _ => format!("/api/v1/threads/{thread_id}/interactions/{UNKNOWN_INTERACTION}/resolve"),
        };
        let path = if route_id == "threads.respondToInteraction" {
            format!("/api/v1/threads/{thread_id}/interactions/{UNKNOWN_INTERACTION}/respond")
        } else {
            path
        };
        let response = request(&fixture.addr, "POST", &path, Some(&body)).await;
        assert_eq!(
            response.status, 422,
            "{route_id} should refuse a body it cannot validate: {}",
            response.body
        );
        assert_error_shape(&response.body);
        assert_eq!(response.body["code"], "invalid_request", "{route_id}");
    }
    fixture.state.shutdown();
}

// --- queued messages --------------------------------------------------------

#[tokio::test]
async fn a_queued_message_is_created_listed_and_snapshotted() {
    let fixture = fixture().await;
    let path = format!("/api/v1/threads/{}/queued-messages", fixture.thread_id);

    let created = fixture
        .post(
            &path,
            json!({ "input": [{ "type": "text", "text": "run the tests" }] }),
        )
        .await;
    assert_eq!(created.status, 201, "{}", created.body);
    assert_response("threads.createQueuedMessage", 201, &created.body);
    assert_eq!(created.body["content"][0]["text"], "run the tests");
    assert_eq!(created.body["initiator"], "user");
    assert_eq!(created.body["payload"]["kind"], "inline");
    assert_eq!(created.body["editable"], true);
    let queued_message_id = created.body["id"].as_str().unwrap().to_owned();
    assert!(queued_message_id.starts_with("qmsg_"));

    // The thread's own read and the global one are the same bare-array shape
    // and both carry the row.
    let listed = fixture.get(&path).await;
    assert_eq!(listed.status, 200, "{}", listed.body);
    assert_response("threads.queuedMessages", 200, &listed.body);
    let rows = listed.body.as_array().unwrap();
    assert_eq!(rows.len(), 1, "{}", listed.body);
    assert_eq!(rows[0]["id"], queued_message_id);

    let global = fixture.get("/api/v1/queued-messages").await;
    assert_eq!(global.status, 200, "{}", global.body);
    assert_response("queue.list", 200, &global.body);
    assert_eq!(global.body.as_array().unwrap().len(), 1, "{}", global.body);

    // A narrowed global read is the same array filtered.
    let narrowed = fixture
        .get(&format!(
            "/api/v1/queued-messages?threadId={}",
            fixture.thread_id
        ))
        .await;
    assert_response("queue.list", 200, &narrowed.body);
    assert_eq!(narrowed.body.as_array().unwrap().len(), 1);

    // The count in the thread row is the queue's real size, not a constant.
    let thread = fixture
        .get(&format!("/api/v1/threads/{}", fixture.thread_id))
        .await;
    assert_eq!(thread.body["queuedMessageCount"], 1, "{}", thread.body);

    // Durability: the entity is in the snapshot an operator restarts from.
    let snapshot = fixture.state.registry.export();
    assert_eq!(snapshot.queued_messages.len(), 1);
    let restored = loom_server::DomainRegistry::new(9_999);
    restored.restore(snapshot.clone());
    assert_eq!(restored.export(), snapshot);
    assert_eq!(
        restored
            .queued_message(&queued_message_id.parse().unwrap())
            .map(|message| message.text),
        Some("run the tests".to_owned())
    );

    fixture.state.shutdown();
}

#[tokio::test]
async fn a_queued_message_refuses_an_input_it_cannot_deliver() {
    let fixture = fixture().await;
    let path = format!("/api/v1/threads/{}/queued-messages", fixture.thread_id);

    // An image cannot be delivered by a provider protocol whose prompt is text.
    // Accepting it and dropping it later is the silent loss the issue forbids.
    let image = fixture
        .post(
            &path,
            json!({ "input": [{ "type": "image", "url": "https://example.com/a.png" }] }),
        )
        .await;
    assert_eq!(image.status, 400, "{}", image.body);
    assert_error(400, &image.body);
    assert_eq!(image.body["code"], "invalid_request");

    let empty = fixture
        .post(
            &path,
            json!({ "input": [{ "type": "text", "text": "   " }] }),
        )
        .await;
    assert_eq!(empty.status, 400, "{}", empty.body);
    assert_error(400, &empty.body);

    // Nothing was stored by a refused attempt.
    let listed = fixture.get(&path).await;
    assert!(
        listed.body.as_array().unwrap().is_empty(),
        "{}",
        listed.body
    );

    // An unknown thread is a 404, not an orphaned row.
    let unknown = fixture
        .post(
            &format!("/api/v1/threads/{UNKNOWN_THREAD}/queued-messages"),
            json!({ "input": [{ "type": "text", "text": "x" }] }),
        )
        .await;
    assert_eq!(unknown.status, 404, "{}", unknown.body);
    assert_error(404, &unknown.body);

    fixture.state.shutdown();
}

#[tokio::test]
async fn sending_a_queued_message_while_busy_answers_the_queued_branch() {
    let fixture = fixture().await;
    let path = format!("/api/v1/threads/{}/queued-messages", fixture.thread_id);

    // Put the thread in flight the way the send route does.
    let sent = fixture
        .post(
            &format!("/api/v1/threads/{}/send", fixture.thread_id),
            json!({ "input": [{ "type": "text", "text": "start" }], "mode": "auto" }),
        )
        .await;
    assert_eq!(sent.status, 200, "{}", sent.body);
    assert_eq!(sent.body["delivery"], "sent");

    let created = fixture
        .post(
            &path,
            json!({ "input": [{ "type": "text", "text": "then this" }] }),
        )
        .await;
    assert_eq!(created.status, 201, "{}", created.body);
    let queued_message_id = created.body["id"].as_str().unwrap().to_owned();

    // A manual `auto` send into a busy thread is the contract's queued branch.
    let again = fixture
        .post(
            &format!(
                "/api/v1/threads/{}/queued-messages/{queued_message_id}/send",
                fixture.thread_id
            ),
            json!({ "mode": "auto" }),
        )
        .await;
    assert_eq!(again.status, 200, "{}", again.body);
    assert_response("threads.sendQueuedMessage", 200, &again.body);
    assert_eq!(again.body["delivery"], "queued");
    assert_eq!(
        again.body["queuedMessage"]["waitingOn"]["kind"],
        "thread-busy"
    );

    // `steer` is refused rather than downgraded: the provider protocol has no
    // frame that injects input into a running turn.
    let steer = fixture
        .post(
            &format!(
                "/api/v1/threads/{}/queued-messages/{queued_message_id}/send",
                fixture.thread_id
            ),
            json!({ "mode": "steer" }),
        )
        .await;
    assert_eq!(steer.status, 501, "{}", steer.body);
    assert_error(501, &steer.body);
    assert_eq!(steer.body["code"], "not_configured");

    fixture.state.shutdown();
}

#[tokio::test]
async fn the_queue_drains_when_the_thread_returns_to_idle() {
    let fixture = fixture().await;
    let path = format!("/api/v1/threads/{}/queued-messages", fixture.thread_id);

    let sent = fixture
        .post(
            &format!("/api/v1/threads/{}/send", fixture.thread_id),
            json!({ "input": [{ "type": "text", "text": "start" }], "mode": "auto" }),
        )
        .await;
    assert_eq!(sent.status, 200, "{}", sent.body);

    let created = fixture
        .post(
            &path,
            json!({ "input": [{ "type": "text", "text": "then this" }] }),
        )
        .await;
    assert_eq!(created.status, 201, "{}", created.body);

    // Reap the running turn the way the reconciler does; the queued message is
    // delivered by the terminal path, not by another client action.
    fixture.state.stop_thread(&fixture.thread_id());
    let listed = fixture.get(&path).await;
    assert!(
        listed.body.as_array().unwrap().is_empty(),
        "the queue should have drained into a new run: {}",
        listed.body
    );
    let thread = fixture
        .get(&format!("/api/v1/threads/{}", fixture.thread_id))
        .await;
    assert_eq!(thread.body["status"], "active", "{}", thread.body);

    fixture.state.shutdown();
}

#[tokio::test]
async fn the_drain_keeps_the_queue_in_order() {
    let fixture = fixture().await;
    let path = format!("/api/v1/threads/{}/queued-messages", fixture.thread_id);

    // A busy thread, so both messages are queued rather than sent.
    fixture
        .post(
            &format!("/api/v1/threads/{}/send", fixture.thread_id),
            json!({ "input": [{ "type": "text", "text": "start" }], "mode": "auto" }),
        )
        .await;
    for text in ["first", "second"] {
        let created = fixture
            .post(
                &path,
                json!({ "input": [{ "type": "text", "text": text }] }),
            )
            .await;
        assert_eq!(created.status, 201, "{}", created.body);
    }

    // Stop the turn: exactly the head of the queue is delivered, because the
    // second message must wait for the run the first one starts. Delivering
    // both would reorder (or double-run) the conversation against the order
    // the client arranged.
    fixture.state.stop_thread(&fixture.thread_id());
    let listed = fixture.get(&path).await;
    let rows = listed.body.as_array().unwrap();
    assert_eq!(
        rows.len(),
        1,
        "one message should still be queued: {}",
        listed.body
    );

    let timeline = fixture
        .get(&format!("/api/v1/threads/{}/timeline", fixture.thread_id))
        .await;
    let texts = timeline.body["rows"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["kind"] == "conversation" && row["role"] == "user")
        .filter_map(|row| row["text"].as_str())
        .collect::<Vec<_>>();
    assert_eq!(texts, vec!["start", "first"], "{}", timeline.body);

    fixture.state.shutdown();
}

#[tokio::test]
async fn a_send_of_an_unknown_queued_message_is_refused() {
    let fixture = fixture().await;
    let response = fixture
        .post(
            &format!(
                "/api/v1/threads/{}/queued-messages/{UNKNOWN_QUEUED_MESSAGE}/send",
                fixture.thread_id
            ),
            json!({ "mode": "auto" }),
        )
        .await;
    assert_eq!(response.status, 404, "{}", response.body);
    assert_error(404, &response.body);
    fixture.state.shutdown();
}

#[tokio::test]
async fn threads_send_queues_while_busy_and_steers_are_refused() {
    let fixture = fixture().await;
    let path = format!("/api/v1/threads/{}/send", fixture.thread_id);

    let first = fixture
        .post(
            &path,
            json!({ "input": [{ "type": "text", "text": "one" }], "mode": "start" }),
        )
        .await;
    assert_eq!(first.status, 200, "{}", first.body);
    assert_eq!(first.body["delivery"], "sent");

    // `auto` while busy becomes a durable queue entry and says so.
    let auto = fixture
        .post(
            &path,
            json!({ "input": [{ "type": "text", "text": "two" }], "mode": "auto" }),
        )
        .await;
    assert_eq!(auto.status, 200, "{}", auto.body);
    assert_response("threads.send", 200, &auto.body);
    assert_eq!(auto.body["delivery"], "queued");
    assert_eq!(
        auto.body["queuedMessage"]["waitingOn"]["kind"],
        "thread-busy"
    );

    // `steer-if-active` while busy is the one that must refuse: it asked to
    // inject into the running turn.
    let steer = fixture
        .post(
            &path,
            json!({ "input": [{ "type": "text", "text": "three" }], "mode": "steer-if-active" }),
        )
        .await;
    assert_eq!(steer.status, 501, "{}", steer.body);
    assert_error(501, &steer.body);
    assert_eq!(steer.body["code"], "not_configured");

    // `start` while busy asked for a turn now and cannot have one.
    let start = fixture
        .post(
            &path,
            json!({ "input": [{ "type": "text", "text": "four" }], "mode": "start" }),
        )
        .await;
    assert_eq!(start.status, 501, "{}", start.body);
    assert_error(501, &start.body);

    fixture.state.shutdown();
}

#[tokio::test]
async fn a_scheduled_send_is_queued_with_a_time_reason() {
    let fixture = fixture().await;
    let path = format!("/api/v1/threads/{}/send", fixture.thread_id);
    let send_at = loom_relay::now_ms() + 60_000;

    let response = fixture
        .post(
            &path,
            json!({
                "input": [{ "type": "text", "text": "later" }],
                "mode": "start",
                "sendAt": send_at,
            }),
        )
        .await;
    assert_eq!(response.status, 200, "{}", response.body);
    assert_response("threads.send", 200, &response.body);
    assert_eq!(response.body["delivery"], "queued");
    assert_eq!(response.body["queuedMessage"]["sendAt"], send_at);
    assert_eq!(response.body["queuedMessage"]["waitingOn"]["kind"], "time");

    // The drain leaves it alone until it is due.
    fixture.state.drain_thread_queue(&fixture.thread_id());
    let listed = fixture
        .get(&format!(
            "/api/v1/threads/{}/queued-messages",
            fixture.thread_id
        ))
        .await;
    assert_eq!(listed.body.as_array().unwrap().len(), 1, "{}", listed.body);

    fixture.state.shutdown();
}

// --- interactions -----------------------------------------------------------

#[tokio::test]
async fn interactions_are_listed_fetched_resolved_and_cancelled() {
    let fixture = fixture().await;
    let approval = fixture.record_interaction(InteractionKind::Approval);
    let question = fixture.record_interaction(InteractionKind::UserQuestion);
    let generic = fixture.record_interaction(InteractionKind::Generic);
    let base = format!("/api/v1/threads/{}", fixture.thread_id);

    let listed = fixture.get(&format!("{base}/interactions")).await;
    assert_eq!(listed.status, 200, "{}", listed.body);
    assert_response("threads.interactions", 200, &listed.body);
    let rows = listed.body.as_array().unwrap();
    assert_eq!(rows.len(), 3, "{}", listed.body);
    assert_eq!(rows[0]["status"], "pending");
    assert_eq!(rows[0]["payload"]["kind"], "approval");

    // The single fetch answers the same row.
    let fetched = fixture
        .get(&format!("{base}/interactions/{}", approval.id))
        .await;
    assert_eq!(fetched.status, 200, "{}", fetched.body);
    assert_response("threads.interaction", 200, &fetched.body);
    assert_eq!(fetched.body["id"], approval.id.to_string());
    assert_eq!(fetched.body["resolution"], Value::Null);

    // `resolve` is the typed verb.
    let resolved = fixture
        .post(
            &format!("{base}/interactions/{}/resolve", approval.id),
            json!({
                "decision": "allow_once",
                "grantedPermissions": {
                    "network": { "enabled": true },
                    "fileSystem": { "read": [], "write": [] },
                },
            }),
        )
        .await;
    assert_eq!(resolved.status, 200, "{}", resolved.body);
    assert_response("threads.resolveInteraction", 200, &resolved.body);
    assert_eq!(resolved.body["status"], "resolved");
    assert_eq!(resolved.body["resolution"]["decision"], "allow_once");
    assert!(resolved.body["resolvedAt"].is_number());

    // `respond` is the opaque verb, and it only answers what loom cannot
    // interpret.
    let responded = fixture
        .post(
            &format!("{base}/interactions/{}/respond", generic.id),
            json!({ "value": { "any": "shape" } }),
        )
        .await;
    assert_eq!(responded.status, 200, "{}", responded.body);
    assert_response("threads.respondToInteraction", 200, &responded.body);
    assert_eq!(responded.body["status"], "resolved");
    assert_eq!(responded.body["resolution"]["kind"], "request_answer");

    // `cancel` settles without an answer.
    let cancelled = fixture
        .post(
            &format!("{base}/interactions/{}/cancel", question.id),
            json!({}),
        )
        .await;
    assert_eq!(cancelled.status, 200, "{}", cancelled.body);
    assert_response("threads.cancelInteraction", 200, &cancelled.body);
    assert_eq!(cancelled.body["status"], "interrupted");
    assert_eq!(cancelled.body["resolution"], Value::Null);

    // The thread list's pending flag is now false: nothing is left open.
    let thread = fixture
        .get(&format!("/api/v1/threads/{}", fixture.thread_id))
        .await;
    let thread_id = thread.body["id"].as_str().unwrap().to_owned();
    let rows = fixture.get("/api/v1/threads").await;
    let row = rows
        .body
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"].as_str() == Some(thread_id.as_str()))
        .unwrap();
    assert_eq!(row["hasPendingInteraction"], false, "{}", row);

    fixture.state.shutdown();
}

#[tokio::test]
async fn a_typed_resolution_must_match_the_interaction() {
    let fixture = fixture().await;
    let approval = fixture.record_interaction(InteractionKind::Approval);
    let question = fixture.record_interaction(InteractionKind::UserQuestion);
    let base = format!("/api/v1/threads/{}", fixture.thread_id);

    // A question's answers cannot answer an approval.
    let wrong = fixture
        .post(
            &format!("{base}/interactions/{}/resolve", approval.id),
            json!({ "kind": "user_answer", "answers": { "q1": { "selected": ["a"] } } }),
        )
        .await;
    assert_eq!(wrong.status, 400, "{}", wrong.body);
    assert_error(400, &wrong.body);
    assert_eq!(wrong.body["code"], "invalid_request");

    // A permission decision cannot answer a question either.
    let wrong = fixture
        .post(
            &format!("{base}/interactions/{}/resolve", question.id),
            json!({ "decision": "deny" }),
        )
        .await;
    assert_eq!(wrong.status, 400, "{}", wrong.body);
    assert_error(400, &wrong.body);

    // A `deny` that carries permissions is outside the resolution *union's*
    // intent, but the exported request schema for this route does not set
    // `additionalProperties: false` on the decision branches, so the middleware
    // passes it and the handler's own coherence check refuses it: `400`, a
    // status the contract's `invalid_request` declares. (The handler keeps the
    // rule because the contract's validator deliberately does not enforce every
    // keyword.)
    let incoherent = fixture
        .post(
            &format!("{base}/interactions/{}/resolve", approval.id),
            json!({
                "decision": "deny",
                "grantedPermissions": { "network": null, "fileSystem": null },
            }),
        )
        .await;
    assert_eq!(incoherent.status, 400, "{}", incoherent.body);
    assert_error(400, &incoherent.body);
    assert_eq!(incoherent.body["code"], "invalid_request");

    // An `allow` with no permissions, and an unknown decision, are outside the
    // schema itself: the middleware answers `422` before the handler.
    for outside in [
        json!({ "decision": "allow_once" }),
        json!({ "decision": "maybe" }),
    ] {
        let response = fixture
            .post(
                &format!("{base}/interactions/{}/resolve", approval.id),
                outside.clone(),
            )
            .await;
        assert_eq!(response.status, 422, "{}: {}", outside, response.body);
        assert_error_shape(&response.body);
        assert_eq!(response.body["code"], "invalid_request");
    }

    // `respond` refuses a typed interaction rather than storing the wrong
    // shape, which is what keeps it distinct from `resolve`.
    let opaque = fixture
        .post(
            &format!("{base}/interactions/{}/respond", approval.id),
            json!({ "value": "yes" }),
        )
        .await;
    assert_eq!(opaque.status, 400, "{}", opaque.body);
    assert_error(400, &opaque.body);

    // Nothing was settled by any refused attempt.
    for interaction in [&approval, &question] {
        let fetched = fixture
            .get(&format!("{base}/interactions/{}", interaction.id))
            .await;
        assert_eq!(fetched.body["status"], "pending", "{}", fetched.body);
    }

    fixture.state.shutdown();
}

#[tokio::test]
async fn a_settled_interaction_refuses_a_second_answer() {
    let fixture = fixture().await;
    let approval = fixture.record_interaction(InteractionKind::Approval);
    let base = format!("/api/v1/threads/{}", fixture.thread_id);

    let first = fixture
        .post(
            &format!("{base}/interactions/{}/resolve", approval.id),
            json!({
                "decision": "allow_once",
                "grantedPermissions": {
                    "network": null,
                    "fileSystem": { "read": [], "write": [] },
                },
            }),
        )
        .await;
    assert_eq!(first.status, 200, "{}", first.body);

    // The second answer is a race with the first, not a no-op.
    let second = fixture
        .post(
            &format!("{base}/interactions/{}/resolve", approval.id),
            json!({ "decision": "deny" }),
        )
        .await;
    assert_eq!(second.status, 409, "{}", second.body);
    assert_error(409, &second.body);
    assert_eq!(second.body["code"], "awaiting_user_interaction");

    // Cancelling an already-resolved interaction is the same race.
    let cancel = fixture
        .post(
            &format!("{base}/interactions/{}/cancel", approval.id),
            json!({}),
        )
        .await;
    assert_eq!(cancel.status, 409, "{}", cancel.body);
    assert_error(409, &cancel.body);

    fixture.state.shutdown();
}

#[tokio::test]
async fn a_plugin_interaction_validates_as_its_own_union_branch() {
    let fixture = fixture().await;
    let plugin = fixture.record_interaction(InteractionKind::Plugin);
    let base = format!("/api/v1/threads/{}", fixture.thread_id);

    // The plugin variant of the interaction union is shaped differently from
    // the provider one — it has no `providerId`/`providerThreadId`/
    // `providerRequestId` at all, and `additionalProperties: false` means
    // leaving them in rejects the row.
    let listed = fixture.get(&format!("{base}/interactions")).await;
    assert_eq!(listed.status, 200, "{}", listed.body);
    assert_response("threads.interactions", 200, &listed.body);
    let row = &listed.body[0];
    assert!(row.get("providerId").is_none(), "{}", row);
    assert_eq!(row["origin"]["kind"], "plugin");
    assert_eq!(row["payload"]["kind"], "plugin");

    // A plugin request is answered by a plugin submission...
    let submitted = fixture
        .post(
            &format!("{base}/interactions/{}/resolve", plugin.id),
            json!({ "kind": "plugin_submitted" }),
        )
        .await;
    assert_eq!(submitted.status, 200, "{}", submitted.body);
    assert_response("threads.resolveInteraction", 200, &submitted.body);
    assert_eq!(submitted.body["resolution"]["kind"], "plugin_submitted");

    fixture.state.shutdown();
}

#[tokio::test]
async fn a_finished_turn_settles_the_interactions_it_left_open() {
    let fixture = fixture().await;
    fixture.record_interaction(InteractionKind::Approval);

    fixture
        .post(
            &format!("/api/v1/threads/{}/send", fixture.thread_id),
            json!({ "input": [{ "type": "text", "text": "start" }], "mode": "auto" }),
        )
        .await;

    // Stopping ends the turn; a terminal turn cannot still be waiting on an
    // answer, so the interaction is settled with it.
    fixture.state.stop_thread(&fixture.thread_id());
    let listed = fixture
        .get(&format!(
            "/api/v1/threads/{}/interactions",
            fixture.thread_id
        ))
        .await;
    assert_eq!(listed.body[0]["status"], "interrupted", "{}", listed.body);

    let rows = fixture.get("/api/v1/threads").await;
    assert_eq!(
        rows.body[0]["hasPendingInteraction"], false,
        "{}",
        rows.body
    );

    fixture.state.shutdown();
}

#[tokio::test]
async fn the_interaction_state_machine_is_durable() {
    let fixture = fixture().await;
    let approval = fixture.record_interaction(InteractionKind::Approval);

    let snapshot = fixture.state.registry.export();
    assert_eq!(snapshot.interactions.len(), 1);
    let restored = loom_server::DomainRegistry::new(9_999);
    restored.restore(snapshot.clone());
    assert_eq!(restored.export(), snapshot);

    let restored_interaction = restored.interaction(&approval.id).unwrap();
    assert_eq!(
        restored_interaction.status,
        loom_domain::InteractionStatus::Pending
    );
    assert_eq!(
        restored_interaction.origin, approval.origin,
        "a pending interaction survives a restart with its identity"
    );

    fixture.state.shutdown();
}

#[tokio::test]
async fn an_interaction_belonging_to_another_thread_is_not_found() {
    let fixture = fixture().await;
    let approval = fixture.record_interaction(InteractionKind::Approval);

    let cases = [
        format!("{UNKNOWN_THREAD}/interactions"),
        format!("{UNKNOWN_THREAD}/interactions/{}", approval.id),
    ];
    for suffix in cases {
        let path = format!("/api/v1/threads/{suffix}");
        let response = fixture.get(&path).await;
        assert_eq!(response.status, 404, "GET {path}: {}", response.body);
        assert_error(404, &response.body);
    }

    // The interaction exists, but not under this thread.
    let other = format!(
        "/api/v1/threads/{UNKNOWN_THREAD}/interactions/{}",
        approval.id
    );
    let response = fixture.get(&other).await;
    assert_eq!(response.status, 404, "{}", response.body);

    let unknown = fixture
        .post(
            &format!(
                "/api/v1/threads/{}/interactions/{UNKNOWN_INTERACTION}/resolve",
                fixture.thread_id
            ),
            json!({ "decision": "deny" }),
        )
        .await;
    assert_eq!(unknown.status, 404, "{}", unknown.body);

    fixture.state.shutdown();
}

// --- plans, goals and the event wait ---------------------------------------

#[tokio::test]
async fn cancel_plan_and_clear_context_refuse_instead_of_pretending() {
    let fixture = fixture().await;
    let base = format!("/api/v1/threads/{}", fixture.thread_id);

    for (path, route_id) in [
        (format!("{base}/plan/cancel"), "threads.cancelPlan"),
        (format!("{base}/context/clear"), "threads.clearContext"),
    ] {
        let response = fixture.post(&path, json!({})).await;
        assert_eq!(response.status, 501, "{route_id}: {}", response.body);
        assert_error(501, &response.body);
        assert_eq!(response.body["code"], "not_configured", "{route_id}");
    }

    // Unknown threads are a 404 before any refusal, like every other route.
    for suffix in [
        "/plan/cancel",
        "/context/clear",
        "/goal/clear",
        "/events/wait",
    ] {
        let path = format!("/api/v1/threads/{UNKNOWN_THREAD}{suffix}");
        let response = if suffix == "/events/wait" {
            fixture.get(&path).await
        } else {
            fixture.post(&path, json!({})).await
        };
        assert_eq!(response.status, 404, "POST {path}: {}", response.body);
        assert_error(404, &response.body);
    }

    fixture.state.shutdown();
}

#[tokio::test]
async fn clear_goal_publishes_the_clearing_event_the_client_projects() {
    let fixture = fixture().await;
    let base = format!("/api/v1/threads/{}", fixture.thread_id);

    // A goal is not a stored field: it is a projection of the thread's run log.
    // Publishing `thread/goal/updated` is what sets it, so the test does that
    // the same way a daemon report would.
    fixture
        .state
        .publish_domain_event(&loom_domain::DomainEvent::ThreadRunEvent {
            run: Box::new(loom_domain::RunEvent::new(
                fixture.thread_id(),
                fixture
                    .state
                    .registry
                    .thread(&fixture.thread_id())
                    .unwrap()
                    .project_id,
                loom_domain::RunId::mint(),
                loom_relay::now_ms(),
                loom_domain::ProviderEvent::ThreadGoalUpdated {
                    provider_thread_id: fixture.thread_id.clone(),
                    objective: "ship B3".into(),
                    status: loom_domain::GoalStatus::Active,
                    time_used_seconds: 1.0,
                    token_budget: None,
                    tokens_used: 10.0,
                },
            )),
        })
        .unwrap();

    let timeline = fixture.get(&format!("{base}/timeline")).await;
    assert_eq!(
        timeline.body["goal"]["objective"], "ship B3",
        "{}",
        timeline.body
    );

    let cleared = fixture.post(&format!("{base}/goal/clear"), json!({})).await;
    assert_eq!(cleared.status, 200, "{}", cleared.body);
    assert_response("threads.clearGoal", 200, &cleared.body);
    assert_eq!(cleared.body["ok"], true);

    let timeline = fixture.get(&format!("{base}/timeline")).await;
    assert_eq!(timeline.body["goal"], Value::Null, "{}", timeline.body);

    fixture.state.shutdown();
}

#[tokio::test]
async fn event_wait_returns_null_on_timeout_and_a_row_on_a_match() {
    let fixture = fixture().await;
    let base = format!("/api/v1/threads/{}", fixture.thread_id);

    // Nothing has happened: the wait honours its bound and answers `null`, the
    // contract's declared shape for "no new event".
    let empty = fixture
        .get(&format!(
            "{base}/events/wait?type=thread%2Fstarted&waitMs=50"
        ))
        .await;
    assert_eq!(empty.status, 200, "{}", empty.body);
    assert_response("threads.eventWait", 200, &empty.body);
    assert_eq!(empty.body, Value::Null);

    // Publish a run event and wait again: the row comes back, and it is the
    // same ThreadEventRow `threads.events` answers.
    let before = fixture.get(&format!("{base}/events")).await;
    let after_seq = before
        .body
        .as_array()
        .and_then(|rows| rows.last())
        .and_then(|row| row["seq"].as_u64())
        .unwrap_or(0);

    // A run event, the shape a daemon report becomes.
    let thread_id = fixture.thread_id();
    let project_id = fixture
        .state
        .registry
        .thread(&thread_id)
        .unwrap()
        .project_id;
    fixture
        .state
        .publish_domain_event(&loom_domain::DomainEvent::ThreadRunEvent {
            run: Box::new(loom_domain::RunEvent::new(
                thread_id,
                project_id,
                loom_domain::RunId::mint(),
                loom_relay::now_ms(),
                loom_domain::ProviderEvent::ThreadIdentity {
                    provider_thread_id: fixture.thread_id.clone(),
                },
            )),
        })
        .unwrap();

    let waited = fixture
        .get(&format!(
            "{base}/events/wait?type=thread%2Fidentity&afterSeq={after_seq}&waitMs=2000"
        ))
        .await;
    assert_eq!(waited.status, 200, "{}", waited.body);
    assert!(
        waited.body["seq"].as_u64().unwrap() > after_seq,
        "the wait must answer a row after the cursor: {}",
        waited.body
    );
    assert_eq!(waited.body["threadId"], fixture.thread_id);
    // The outer row shape is the contract's `ThreadEventRow`, whose exported
    // `allOf` of `{id,scope,threadId,seq,createdAt}` and `{type,data}` combines
    // `additionalProperties: false` on both halves — unsatisfiable under JSON
    // Schema semantics, which is why `threads.events` rows are asserted the
    // same way (`http.rs::b1_routes_return_contract_conformant_responses`):
    // frame the fields by hand and validate the *inner* contract event, which
    // is the part a projection actually consumes.
    for field in [
        "id",
        "scope",
        "threadId",
        "seq",
        "createdAt",
        "type",
        "data",
    ] {
        assert!(
            !waited.body[field].is_null(),
            "{field} is missing: {}",
            waited.body
        );
    }
    let mut inner = waited.body["data"].clone();
    let inner_object = inner.as_object_mut().expect("event data object");
    inner_object.insert("threadId".into(), waited.body["threadId"].clone());
    inner_object.insert("scope".into(), waited.body["scope"].clone());
    inner_object.insert("type".into(), waited.body["type"].clone());
    let violations = shared().validate_thread_event(&inner);
    assert!(
        violations.is_empty(),
        "the waited row's ThreadEvent is invalid: {violations:?}\n{inner}"
    );

    // A still-not-due wait with a tiny bound answers `null` again, which is
    // the contract's declared shape for "nothing more happened".
    let latest = waited.body["seq"].as_u64().unwrap();
    let drained = fixture
        .get(&format!(
            "{base}/events/wait?type=thread%2Fidentity&afterSeq={latest}&waitMs=50"
        ))
        .await;
    assert_eq!(drained.status, 200, "{}", drained.body);
    assert_response("threads.eventWait", 200, &drained.body);
    assert_eq!(drained.body, Value::Null);

    fixture.state.shutdown();
}

#[tokio::test]
async fn turn_summary_details_are_a_filtered_view_of_the_timeline() {
    let fixture = fixture().await;
    let base = format!("/api/v1/threads/{}", fixture.thread_id);

    fixture
        .post(
            &format!("{base}/send"),
            json!({ "input": [{ "type": "text", "text": "hello" }], "mode": "auto" }),
        )
        .await;
    fixture
        .state
        .registry
        .post_message(
            &fixture.thread_id(),
            MessageRole::Assistant,
            "hi there".into(),
            loom_relay::now_ms(),
        )
        .unwrap();

    let timeline = fixture.get(&format!("{base}/timeline")).await;
    let rows = timeline.body["rows"].as_array().unwrap();
    assert!(!rows.is_empty());
    let first = rows.first().unwrap();
    let start = first["sourceSeqStart"].as_u64().unwrap();
    let last = rows.last().unwrap();
    let end = last["sourceSeqEnd"].as_u64().unwrap();

    let details = fixture
        .get(&format!(
            "{base}/timeline/turn-summary-details?turnId=turn-1&sourceSeqStart={start}&sourceSeqEnd={end}"
        ))
        .await;
    assert_eq!(details.status, 200, "{}", details.body);
    assert_response("threads.timelineTurnSummaryDetails", 200, &details.body);
    assert_eq!(details.body["historySnapshot"], Value::Null);
    assert!(!details.body["rows"].as_array().unwrap().is_empty());

    // Every returned row is byte-identical to the timeline's own row: this is a
    // selection, not a second projection.
    for row in details.body["rows"].as_array().unwrap() {
        let id = row["id"].as_str().unwrap();
        let source = rows
            .iter()
            .find(|candidate| candidate["id"].as_str() == Some(id))
            .unwrap_or_else(|| panic!("row {id} is not a timeline row"));
        assert_eq!(row, source, "the filtered row differs from the timeline's");
    }

    // A range with no rows is an empty array, not an error.
    let empty = fixture
        .get(&format!(
            "{base}/timeline/turn-summary-details?turnId=turn-1&sourceSeqStart=99998&sourceSeqEnd=99999"
        ))
        .await;
    assert_eq!(empty.status, 200, "{}", empty.body);
    assert_response("threads.timelineTurnSummaryDetails", 200, &empty.body);
    assert!(empty.body["rows"].as_array().unwrap().is_empty());

    // A cursor that is not a row of the range is refused rather than silently
    // starting from the beginning.
    let bad_cursor = fixture
        .get(&format!(
            "{base}/timeline/turn-summary-details?turnId=turn-1&sourceSeqStart={start}&sourceSeqEnd={end}&beforeCursor=nope"
        ))
        .await;
    assert_eq!(bad_cursor.status, 400, "{}", bad_cursor.body);
    assert_error(400, &bad_cursor.body);

    fixture.state.shutdown();
}

// --- retry's queued branch --------------------------------------------------

#[tokio::test]
async fn a_scheduled_retry_is_queued_with_a_retry_payload() {
    let fixture = fixture().await;
    let base = format!("/api/v1/threads/{}", fixture.thread_id);

    // A turn to retry.
    let sent = fixture
        .post(
            &format!("{base}/send"),
            json!({ "input": [{ "type": "text", "text": "check the deploy" }], "mode": "auto" }),
        )
        .await;
    assert_eq!(sent.status, 200, "{}", sent.body);

    // While the turn is in flight, a retry is queued rather than refused.
    let queued = fixture
        .post(
            &format!("{base}/retry"),
            json!({ "reason": "the first attempt timed out" }),
        )
        .await;
    assert_eq!(queued.status, 200, "{}", queued.body);
    assert_response("threads.retry", 200, &queued.body);
    assert_eq!(queued.body["delivery"], "queued");
    assert_eq!(queued.body["attempt"], 1);
    assert!(queued.body["queuedMessageId"]
        .as_str()
        .unwrap()
        .starts_with("qmsg_"));
    assert_eq!(queued.body["waitingOn"]["kind"], "thread-busy");

    let listed = fixture.get(&format!("{base}/queued-messages")).await;
    let rows = listed.body.as_array().unwrap();
    assert_eq!(rows.len(), 1, "{}", listed.body);
    assert_eq!(rows[0]["payload"]["kind"], "retry");
    assert_eq!(rows[0]["payload"]["attempt"], 1);
    assert_eq!(rows[0]["payload"]["reason"], "the first attempt timed out");

    // A future `sendAt` is the same branch with the time reason.
    let scheduled = fixture
        .post(
            &format!("{base}/retry"),
            json!({ "sendAt": loom_relay::now_ms() + 60_000, "reason": "later" }),
        )
        .await;
    assert_eq!(scheduled.status, 200, "{}", scheduled.body);
    assert_response("threads.retry", 200, &scheduled.body);
    assert_eq!(scheduled.body["delivery"], "queued");
    assert_eq!(scheduled.body["waitingOn"]["kind"], "time");

    fixture.state.shutdown();
}

#[tokio::test]
async fn a_retry_of_a_thread_with_no_turn_still_refuses() {
    let fixture = fixture().await;
    let response = fixture
        .post(
            &format!("/api/v1/threads/{}/retry", fixture.thread_id),
            json!({}),
        )
        .await;
    assert_eq!(response.status, 409, "{}", response.body);
    assert_error(409, &response.body);
    assert_eq!(response.body["code"], "no_failed_turn");
    fixture.state.shutdown();
}

// --- the state machine, as the domains define it ---------------------------

/// The two new domains' state machines, exercised through the registry the
/// routes use.
///
/// The transitions are the acceptance criterion "新增域的状态机有测试（合法/非法转
/// 移）", at the level the routes actually reach them: a second send of a sent
/// row, and a cancellation of a settled interaction.
#[tokio::test]
async fn the_two_new_state_machines_refuse_illegal_transitions() {
    let fixture = fixture().await;

    // Queued message: queued -> sent, and sent is terminal.
    let message = fixture
        .state
        .registry
        .create_queued_message(
            loom_domain::NewQueuedMessage {
                thread_id: fixture.thread_id(),
                sender_thread_id: None,
                initiator: loom_domain::QueuedMessageInitiator::User,
                text: "x".into(),
                model: None,
                reasoning_level: None,
                permission_mode: None,
                service_tier: loom_domain::ServiceTier::Default,
                group_with_next: false,
                send_at: None,
                payload: loom_domain::QueuedMessagePayload::Inline,
            },
            1,
        )
        .unwrap()
        .0;
    fixture
        .state
        .registry
        .mark_queued_message_sent(&message.id, 2)
        .unwrap();
    assert!(fixture
        .state
        .registry
        .mark_queued_message_sent(&message.id, 3)
        .is_err());
    assert!(fixture
        .state
        .registry
        .cancel_queued_message(&message.id, 3)
        .is_err());

    // Cancelling from queued is legal, and cancelled is terminal too.
    let cancellable = fixture
        .state
        .registry
        .create_queued_message(
            loom_domain::NewQueuedMessage {
                thread_id: fixture.thread_id(),
                sender_thread_id: None,
                initiator: loom_domain::QueuedMessageInitiator::User,
                text: "y".into(),
                model: None,
                reasoning_level: None,
                permission_mode: None,
                service_tier: loom_domain::ServiceTier::Default,
                group_with_next: false,
                send_at: None,
                payload: loom_domain::QueuedMessagePayload::Inline,
            },
            1,
        )
        .unwrap()
        .0;
    fixture
        .state
        .registry
        .cancel_queued_message(&cancellable.id, 2)
        .unwrap();
    assert!(fixture
        .state
        .registry
        .mark_queued_message_sent(&cancellable.id, 3)
        .is_err());

    // Interaction: pending -> resolved, and resolved is terminal. The
    // registry refuses the second write *before* the domain sees it, which is
    // what turns a lost claim into a conflict instead of a silent overwrite.
    let approval = fixture.record_interaction(InteractionKind::Approval);
    fixture
        .state
        .registry
        .resolve_interaction(
            &approval.id,
            Resolution::Decision {
                decision: "deny".into(),
                granted_permissions: None,
            },
            5,
        )
        .unwrap();
    assert!(fixture
        .state
        .registry
        .resolve_interaction(
            &approval.id,
            Resolution::Decision {
                decision: "allow_once".into(),
                granted_permissions: Some(json!(null)),
            },
            6,
        )
        .is_err());
    assert!(fixture
        .state
        .registry
        .cancel_interaction(&approval.id, None, 7)
        .is_err());

    fixture.state.shutdown();
}
