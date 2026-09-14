//! B4 conformance: thread lifecycle, pin/read state and queued-message editing.
//!
//! The tests exercise the public routes through a real TCP listener and validate
//! every successful body against the embedded bb contract. The request cases
//! also run through the contract validator, so a handler cannot quietly accept
//! a loom-specific dialect while still looking successful to a client.

use std::future::Future;
use std::time::Duration;

use loom_contract::shared;
use loom_domain::{EnvironmentKind, QueuedMessageStatus, Thread, ThreadStatus};
use loom_server::http::router;
use loom_server::state::{AppConfig, AppState};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const TIMEOUT: Duration = Duration::from_secs(10);

const UNKNOWN_THREAD: &str = "thr_01M27Y6Q0J8V4W2C7K5N3P1R9Z";
const UNKNOWN_QUEUED_MESSAGE: &str = "qmsg_01M27Y6Q0J8V4W2C7K5N3P1R9Z";

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
    let declared = contract.error_statuses(code);
    assert!(
        !declared.is_empty(),
        "code {code:?} is not in the contract error-code list"
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

struct Fixture {
    addr: String,
    state: AppState,
    project_id: String,
    environment_id: String,
    thread_id: String,
}

async fn fixture() -> Fixture {
    let (addr, state) = spawn_server().await;
    let now = loom_relay::now_ms();
    let (project, _) = state
        .registry
        .create_project("b4".into(), loom_domain::ProjectKind::Standard, None, now)
        .unwrap();
    let (host, _) = state
        .registry
        .enroll_host(None, "b4-host".into(), now)
        .unwrap();
    let (environment, _) = state
        .registry
        .create_environment(
            Some(project.id.clone()),
            host.id.clone(),
            EnvironmentKind::Unmanaged,
            Some("/srv/b4".into()),
            now,
        )
        .unwrap();
    let (thread, _) = state
        .registry
        .create_thread(
            Some(project.id.clone()),
            Some("b4 thread".into()),
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
    }
}

impl Fixture {
    fn get<'a>(&'a self, path: &'a str) -> impl Future<Output = Response> + 'a {
        request(&self.addr, "GET", path, None)
    }

    fn post(&self, path: &str, body: Value) -> impl Future<Output = Response> + '_ {
        let addr = self.addr.clone();
        let path = path.to_owned();
        async move { request(&addr, "POST", &path, Some(body)).await }
    }

    fn post_empty(&self, path: &str) -> impl Future<Output = Response> + '_ {
        let addr = self.addr.clone();
        let path = path.to_owned();
        async move { request(&addr, "POST", &path, None).await }
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

    fn thread_id(&self) -> loom_domain::ThreadId {
        self.thread_id.parse().unwrap()
    }

    fn new_thread(&self, title: &str) -> Thread {
        self.state
            .registry
            .create_thread(
                Some(self.project_id.parse().unwrap()),
                Some(title.to_owned()),
                Some(self.environment_id.parse().unwrap()),
                loom_relay::now_ms(),
            )
            .unwrap()
            .0
    }

    async fn queue_message(&self, text: &str) -> Response {
        self.post(
            &format!("/api/v1/threads/{}/queued-messages", self.thread_id),
            json!({ "input": [{ "type": "text", "text": text }] }),
        )
        .await
    }
}

#[test]
fn b4_json_write_requests_are_contract_shaped() {
    let cases = [
        (
            "threads.delete",
            json!({ "childThreadsConfirmed": false }),
            json!({ "child_threads_confirmed": false }),
        ),
        (
            "threads.fork",
            json!({ "sourceThreadId": "thr_source" }),
            json!({ "source_thread_id": "thr_source" }),
        ),
        (
            "threads.pinOrder",
            json!({ "previousThreadId": null, "nextThreadId": null }),
            json!({ "previousThreadId": null }),
        ),
        (
            "threads.resolveMentions",
            json!({ "threadIds": [] }),
            json!({ "thread_ids": [] }),
        ),
        (
            "threads.reorderQueuedMessage",
            json!({ "previousQueuedMessageId": null, "nextQueuedMessageId": null }),
            json!({ "previous_queued_message_id": null, "next_queued_message_id": null }),
        ),
        (
            "threads.setQueuedMessageGroupBoundary",
            json!({
                "expectedGroupedPrefixQueuedMessageIds": ["qmsg_1"],
                "groupBoundaryQueuedMessageId": "qmsg_2"
            }),
            json!({
                "expectedGroupedPrefixQueuedMessageIds": [],
                "groupBoundaryQueuedMessageId": "qmsg_2"
            }),
        ),
        (
            "threads.updateQueuedMessage",
            json!({
                "expectedUpdatedAt": 0,
                "input": [{ "type": "text", "text": "updated" }]
            }),
            json!({ "expectedUpdatedAt": 0, "input": [] }),
        ),
    ];

    let contract = shared();
    let delete = &cases[0].1;
    assert!(contract
        .validate_request_by_id("threads.delete", delete)
        .is_empty());
    let fork = &cases[1].1;
    assert!(contract
        .validate_request_by_id("threads.fork", fork)
        .is_empty());
    let pin_order = &cases[2].1;
    assert!(contract
        .validate_request_by_id("threads.pinOrder", pin_order)
        .is_empty());
    let mentions = &cases[3].1;
    assert!(contract
        .validate_request_by_id("threads.resolveMentions", mentions)
        .is_empty());
    let reorder = &cases[4].1;
    assert!(contract
        .validate_request_by_id("threads.reorderQueuedMessage", reorder)
        .is_empty());
    let group_boundary = &cases[5].1;
    assert!(contract
        .validate_request_by_id("threads.setQueuedMessageGroupBoundary", group_boundary)
        .is_empty());
    let update = &cases[6].1;
    assert!(contract
        .validate_request_by_id("threads.updateQueuedMessage", update)
        .is_empty());
    for (route_id, inside, outside) in cases {
        assert!(
            contract
                .validate_request_by_id(route_id, &inside)
                .is_empty(),
            "{route_id} rejects a contract-shaped request: {inside}"
        );
        assert!(
            !contract
                .validate_request_by_id(route_id, &outside)
                .is_empty(),
            "{route_id} accepts a request outside its contract: {outside}"
        );
    }
}

#[tokio::test]
async fn the_live_server_rejects_b4_request_counterexamples() {
    let fixture = fixture().await;
    let thread_id = &fixture.thread_id;
    let cases = [
        (
            "DELETE",
            format!("/api/v1/threads/{thread_id}"),
            json!({ "child_threads_confirmed": false }),
        ),
        (
            "POST",
            "/api/v1/threads/fork".to_owned(),
            json!({ "source_thread_id": "thr_source" }),
        ),
        (
            "PATCH",
            format!("/api/v1/threads/{thread_id}/pin-order"),
            json!({ "previousThreadId": null }),
        ),
        (
            "POST",
            "/api/v1/threads/resolve-mentions".to_owned(),
            json!({ "thread_ids": [] }),
        ),
        (
            "PATCH",
            format!("/api/v1/threads/{thread_id}/queued-messages/{UNKNOWN_QUEUED_MESSAGE}/order"),
            json!({
                "previous_queued_message_id": null,
                "next_queued_message_id": null
            }),
        ),
        (
            "PATCH",
            format!("/api/v1/threads/{thread_id}/queued-messages/group-boundary"),
            json!({
                "expectedGroupedPrefixQueuedMessageIds": [],
                "groupBoundaryQueuedMessageId": "qmsg_2"
            }),
        ),
        (
            "PATCH",
            format!("/api/v1/threads/{thread_id}/queued-messages/{UNKNOWN_QUEUED_MESSAGE}"),
            json!({ "expectedUpdatedAt": 0, "input": [] }),
        ),
    ];
    for (method, path, body) in cases {
        let response = request(&fixture.addr, method, &path, Some(body)).await;
        assert_eq!(response.status, 422, "{method} {path}: {}", response.body);
        assert_error_shape(&response.body);
        assert_eq!(response.body["code"], "invalid_request");
    }
    fixture.state.shutdown();
}

#[tokio::test]
async fn lifecycle_routes_have_contract_bodies_and_real_state_transitions() {
    let fixture = fixture().await;
    let root_id = fixture.thread_id.clone();
    let base = format!("/api/v1/threads/{root_id}");

    let archived = fixture.post_empty(&format!("{base}/archive")).await;
    assert_eq!(archived.status, 200, "{}", archived.body);
    assert_response("threads.archive", 200, &archived.body);
    let archived_thread = fixture.state.registry.thread(&fixture.thread_id()).unwrap();
    assert_eq!(archived_thread.status, ThreadStatus::Archived);
    assert!(archived_thread.archived_at_ms.is_some());

    let archived_view = fixture.get(&base).await;
    assert_eq!(archived_view.status, 200, "{}", archived_view.body);
    assert_response("threads.get", 200, &archived_view.body);
    assert_eq!(archived_view.body["status"], "idle");
    assert!(archived_view.body["archivedAt"].is_number());

    let unarchived = fixture.post_empty(&format!("{base}/unarchive")).await;
    assert_eq!(unarchived.status, 200, "{}", unarchived.body);
    assert_response("threads.unarchive", 200, &unarchived.body);
    let live_thread = fixture.state.registry.thread(&fixture.thread_id()).unwrap();
    assert_eq!(live_thread.status, ThreadStatus::Idle);
    assert_eq!(live_thread.archived_at_ms, None);

    let (child, _) = fixture
        .state
        .registry
        .create_fork_thread(
            &fixture.thread_id(),
            Some("fork child".into()),
            Some(fixture.environment_id.parse().unwrap()),
            None,
            loom_relay::now_ms(),
        )
        .unwrap();
    let archive_all = fixture.post_empty(&format!("{base}/archive-all")).await;
    assert_eq!(archive_all.status, 200, "{}", archive_all.body);
    assert_response("threads.archiveAll", 200, &archive_all.body);
    assert_eq!(archive_all.body["ok"], true);
    let ids = archive_all.body["archivedThreadIds"].as_array().unwrap();
    assert_eq!(ids.len(), 2, "{}", archive_all.body);
    assert_eq!(ids[0], root_id);
    assert_eq!(ids[1], child.id.to_string());
    assert_eq!(
        fixture
            .state
            .registry
            .thread(&fixture.thread_id())
            .unwrap()
            .status,
        ThreadStatus::Archived
    );
    assert_eq!(
        fixture.state.registry.thread(&child.id).unwrap().status,
        ThreadStatus::Archived
    );

    let repeated = fixture.post_empty(&format!("{base}/archive-all")).await;
    assert_eq!(repeated.status, 200, "{}", repeated.body);
    assert_response("threads.archiveAll", 200, &repeated.body);
    assert_eq!(repeated.body["archivedThreadIds"], json!([]));
    fixture.state.shutdown();
}

#[tokio::test]
async fn deleting_a_thread_requires_confirmation_and_leaves_a_tombstone() {
    let fixture = fixture().await;
    let root_id = fixture.thread_id.clone();
    let (child, _) = fixture
        .state
        .registry
        .create_fork_thread(
            &fixture.thread_id(),
            Some("child".into()),
            Some(fixture.environment_id.parse().unwrap()),
            None,
            loom_relay::now_ms(),
        )
        .unwrap();
    let path = format!("/api/v1/threads/{root_id}");

    let denied = fixture
        .delete(&path, Some(json!({ "childThreadsConfirmed": false })))
        .await;
    assert_eq!(denied.status, 409, "{}", denied.body);
    assert_error(409, &denied.body);
    assert_eq!(denied.body["code"], "child_threads_confirmation_required");
    assert_eq!(denied.body["details"]["childThreadCount"], 1);
    assert!(fixture
        .state
        .registry
        .thread(&fixture.thread_id())
        .unwrap()
        .deleted_at_ms
        .is_none());

    let deleted = fixture
        .delete(&path, Some(json!({ "childThreadsConfirmed": true })))
        .await;
    assert_eq!(deleted.status, 200, "{}", deleted.body);
    assert_response("threads.delete", 200, &deleted.body);
    let tombstone = fixture.state.registry.thread(&fixture.thread_id()).unwrap();
    assert!(tombstone.deleted_at_ms.is_some());
    assert!(!fixture
        .state
        .registry
        .threads()
        .iter()
        .any(|thread| thread.id == fixture.thread_id()));

    let hidden = fixture.get(&path).await;
    assert_eq!(hidden.status, 404, "{}", hidden.body);
    assert_error(404, &hidden.body);
    assert_eq!(hidden.body["code"], "thread_not_found");

    let mentions = fixture
        .post(
            "/api/v1/threads/resolve-mentions",
            json!({ "threadIds": [root_id, child.id.to_string(), UNKNOWN_THREAD] }),
        )
        .await;
    assert_eq!(mentions.status, 200, "{}", mentions.body);
    assert_response("threads.resolveMentions", 200, &mentions.body);
    assert_eq!(mentions.body.as_array().unwrap().len(), 1);
    assert_eq!(mentions.body[0]["threadId"], child.id.to_string());

    let snapshot = fixture.state.registry.export();
    assert!(snapshot
        .threads
        .iter()
        .any(|thread| thread.id == fixture.thread_id() && thread.deleted_at_ms.is_some()));
    let restored = loom_server::DomainRegistry::new(9_999);
    restored.restore(snapshot.clone());
    assert_eq!(restored.export(), snapshot);
    assert!(restored.public_thread(&fixture.thread_id()).is_none());
    fixture.state.shutdown();
}

#[tokio::test]
async fn fork_reports_session_and_acp_capability_boundaries_without_creating_a_row() {
    let fixture = fixture().await;
    let source_id = fixture.thread_id.clone();
    let path = "/api/v1/threads/fork";

    let unavailable = fixture
        .post(path, json!({ "sourceThreadId": source_id }))
        .await;
    assert_eq!(unavailable.status, 400, "{}", unavailable.body);
    assert_error(400, &unavailable.body);
    assert_eq!(unavailable.body["code"], "fork_source_session_unavailable");
    assert_eq!(fixture.state.registry.threads().len(), 1);

    fixture
        .state
        .registry
        .set_provider_session_id(
            &fixture.thread_id(),
            "provider-session",
            Some(loom_domain::ProviderSessionBinding::new("pi", "/srv/b4")),
            loom_relay::now_ms(),
        )
        .unwrap();
    let not_configured = fixture
        .post(path, json!({ "sourceThreadId": source_id }))
        .await;
    assert_eq!(not_configured.status, 501, "{}", not_configured.body);
    assert_error(501, &not_configured.body);
    assert_eq!(not_configured.body["code"], "not_configured");
    assert_eq!(fixture.state.registry.threads().len(), 1);

    let archived = fixture
        .post_empty(&format!("/api/v1/threads/{source_id}/archive"))
        .await;
    assert_eq!(archived.status, 200, "{}", archived.body);
    let archived_fork = fixture
        .post(path, json!({ "sourceThreadId": source_id }))
        .await;
    assert_eq!(archived_fork.status, 409, "{}", archived_fork.body);
    assert_error(409, &archived_fork.body);
    assert_eq!(archived_fork.body["code"], "thread_not_writable");
    assert_eq!(fixture.state.registry.threads().len(), 1);
    fixture.state.shutdown();
}

#[tokio::test]
async fn pin_unpin_and_unread_return_rows_and_survive_restore() {
    let fixture = fixture().await;
    let root = fixture.thread_id.clone();
    let second = fixture.new_thread("second");
    let third = fixture.new_thread("third");
    let second_id = second.id.to_string();
    let third_id = third.id.to_string();

    for id in [&root, &second_id, &third_id] {
        let response = fixture
            .post_empty(&format!("/api/v1/threads/{id}/pin"))
            .await;
        assert_eq!(response.status, 200, "{}", response.body);
        assert_response("threads.pin", 200, &response.body);
        assert!(response.body["pinnedAt"].is_number());
    }

    let order = fixture
        .patch(
            &format!("/api/v1/threads/{root}/pin-order"),
            json!({ "previousThreadId": third_id, "nextThreadId": second_id }),
        )
        .await;
    assert_eq!(order.status, 200, "{}", order.body);
    assert_response("threads.pinOrder", 200, &order.body);
    let order_ids: Vec<&str> = order
        .body
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["id"].as_str().expect("thread list row has an id"))
        .collect();
    assert_eq!(
        order_ids,
        vec![third.id.as_str(), root.as_str(), second.id.as_str()]
    );

    let unpinned = fixture
        .post_empty(&format!("/api/v1/threads/{second_id}/unpin"))
        .await;
    assert_eq!(unpinned.status, 200, "{}", unpinned.body);
    assert_response("threads.unpin", 200, &unpinned.body);
    assert_eq!(unpinned.body["pinnedAt"], Value::Null);
    assert_eq!(
        fixture
            .state
            .registry
            .thread(&second.id)
            .unwrap()
            .pin_sort_key,
        None
    );

    let read = fixture
        .post_empty(&format!("/api/v1/threads/{root}/read"))
        .await;
    assert_eq!(read.status, 200, "{}", read.body);
    assert_response("threads.read", 200, &read.body);
    assert!(read.body["lastReadAt"].is_number());
    let unread = fixture
        .post_empty(&format!("/api/v1/threads/{root}/unread"))
        .await;
    assert_eq!(unread.status, 200, "{}", unread.body);
    assert_response("threads.unread", 200, &unread.body);
    assert_eq!(unread.body["lastReadAt"], Value::Null);

    let snapshot = fixture.state.registry.export();
    let restored = loom_server::DomainRegistry::new(9_999);
    restored.restore(snapshot.clone());
    assert_eq!(restored.export(), snapshot);
    assert!(restored
        .thread(&fixture.thread_id())
        .unwrap()
        .pin_sort_key
        .is_some());
    assert!(restored.thread(&third.id).unwrap().pin_sort_key.is_some());
    assert_eq!(restored.thread(&second.id).unwrap().pinned_at_ms, None);
    fixture.state.shutdown();
}

#[tokio::test]
async fn queue_reorder_rebalances_when_the_fractional_key_is_occupied() {
    let fixture = fixture().await;
    let base = format!("/api/v1/threads/{}/queued-messages", fixture.thread_id);
    let first = fixture.queue_message("first").await;
    let second = fixture.queue_message("second").await;
    let third = fixture.queue_message("third").await;
    let fourth = fixture.queue_message("fourth").await;
    let first_id = first.body["id"].as_str().unwrap().to_owned();
    let second_id = second.body["id"].as_str().unwrap().to_owned();
    let third_id = third.body["id"].as_str().unwrap().to_owned();
    let fourth_id = fourth.body["id"].as_str().unwrap().to_owned();

    let reordered = fixture
        .patch(
            &format!("{base}/{fourth_id}/order"),
            json!({
                "previousQueuedMessageId": first_id,
                "nextQueuedMessageId": third_id
            }),
        )
        .await;
    assert_eq!(reordered.status, 200, "{}", reordered.body);
    assert_response("threads.reorderQueuedMessage", 200, &reordered.body);
    assert_eq!(
        reordered
            .body
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec![
            first_id.as_str(),
            fourth_id.as_str(),
            second_id.as_str(),
            third_id.as_str()
        ]
    );

    let keys = fixture
        .state
        .registry
        .queued_messages_for(Some(&fixture.thread_id()))
        .into_iter()
        .map(|message| message.sort_key)
        .collect::<Vec<_>>();
    assert!(keys.windows(2).all(|pair| pair[0] < pair[1]));
    fixture.state.shutdown();
}

#[tokio::test]
async fn queued_messages_support_cas_update_reorder_grouping_delete_and_restore() {
    let fixture = fixture().await;
    let base = format!("/api/v1/threads/{}/queued-messages", fixture.thread_id);
    let first = fixture.queue_message("first").await;
    let second = fixture.queue_message("second").await;
    let third = fixture.queue_message("third").await;
    for response in [&first, &second, &third] {
        assert_eq!(response.status, 201, "{}", response.body);
        assert_response("threads.createQueuedMessage", 201, &response.body);
    }
    let first_id = first.body["id"].as_str().unwrap().to_owned();
    let second_id = second.body["id"].as_str().unwrap().to_owned();
    let third_id = third.body["id"].as_str().unwrap().to_owned();
    let first_updated_at = first.body["updatedAt"].as_u64().unwrap();

    let updated = fixture
        .patch(
            &format!("{base}/{first_id}"),
            json!({
                "expectedUpdatedAt": first_updated_at,
                "input": [{ "type": "text", "text": "updated first" }]
            }),
        )
        .await;
    assert_eq!(updated.status, 200, "{}", updated.body);
    assert_response("threads.updateQueuedMessage", 200, &updated.body);
    assert_eq!(updated.body["content"][0]["text"], "updated first");
    let updated_at = updated.body["updatedAt"].as_u64().unwrap();
    assert!(updated_at > first_updated_at);

    let stale = fixture
        .patch(
            &format!("{base}/{first_id}"),
            json!({
                "expectedUpdatedAt": first_updated_at,
                "input": [{ "type": "text", "text": "lost update" }]
            }),
        )
        .await;
    assert_eq!(stale.status, 409, "{}", stale.body);
    assert_error(409, &stale.body);
    assert_eq!(stale.body["code"], "conflict");

    let reordered = fixture
        .patch(
            &format!("{base}/{third_id}/order"),
            json!({ "previousQueuedMessageId": null, "nextQueuedMessageId": first_id }),
        )
        .await;
    assert_eq!(reordered.status, 200, "{}", reordered.body);
    assert_response("threads.reorderQueuedMessage", 200, &reordered.body);
    let reordered_ids: Vec<&str> = reordered
        .body
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["id"].as_str().expect("queued row has an id"))
        .collect();
    assert_eq!(
        reordered_ids,
        vec![third_id.as_str(), first_id.as_str(), second_id.as_str()]
    );

    let grouped = fixture
        .patch(
            &format!("{base}/group-boundary"),
            json!({
                "expectedGroupedPrefixQueuedMessageIds": [third_id, first_id],
                "groupBoundaryQueuedMessageId": second_id
            }),
        )
        .await;
    assert_eq!(grouped.status, 200, "{}", grouped.body);
    assert_response("threads.setQueuedMessageGroupBoundary", 200, &grouped.body);
    let grouped_rows = grouped.body.as_array().unwrap();
    assert_eq!(grouped_rows.len(), 3);
    assert_eq!(grouped_rows[0]["groupWithNext"], true);
    assert_eq!(grouped_rows[1]["groupWithNext"], false);
    assert_eq!(grouped_rows[2]["groupWithNext"], false);

    let stale_group = fixture
        .patch(
            &format!("{base}/group-boundary"),
            json!({
                "expectedGroupedPrefixQueuedMessageIds": [first_id],
                "groupBoundaryQueuedMessageId": second_id
            }),
        )
        .await;
    assert_eq!(stale_group.status, 409, "{}", stale_group.body);
    assert_error(409, &stale_group.body);
    assert_eq!(stale_group.body["code"], "conflict");

    let deleted = fixture.delete(&format!("{base}/{second_id}"), None).await;
    assert_eq!(deleted.status, 200, "{}", deleted.body);
    assert_response("threads.deleteQueuedMessage", 200, &deleted.body);
    assert_eq!(
        fixture
            .state
            .registry
            .queued_message(&second_id.parse().unwrap())
            .unwrap()
            .status,
        QueuedMessageStatus::Cancelled
    );

    let listed = fixture.get(&base).await;
    assert_eq!(listed.status, 200, "{}", listed.body);
    assert_response("threads.queuedMessages", 200, &listed.body);
    let listed_ids: Vec<&str> = listed
        .body
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["id"].as_str().expect("queued row has an id"))
        .collect();
    assert_eq!(listed_ids, vec![third_id.as_str(), first_id.as_str()]);
    assert_eq!(listed.body.as_array().unwrap().len(), 2);

    let snapshot = fixture.state.registry.export();
    let cancelled = snapshot
        .queued_messages
        .iter()
        .find(|message| message.id == second_id.parse().unwrap())
        .unwrap();
    assert_eq!(cancelled.status, QueuedMessageStatus::Cancelled);
    let restored = loom_server::DomainRegistry::new(9_999);
    restored.restore(snapshot.clone());
    assert_eq!(restored.export(), snapshot);
    assert!(
        !restored
            .queued_message(&second_id.parse().unwrap())
            .unwrap()
            .group_with_next
    );
    assert_eq!(
        restored
            .queued_messages_for(Some(&fixture.thread_id()))
            .iter()
            .map(|message| message.id.to_string())
            .collect::<Vec<_>>(),
        vec![third_id, first_id, second_id]
    );
    fixture.state.shutdown();
}
