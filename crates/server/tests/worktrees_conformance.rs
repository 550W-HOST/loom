//! Managed-worktree conformance at the HTTP surface.
//!
//! No worker here: these tests pin what the control plane promises about a
//! `git-worktree` environment before and after a host acts — the provider
//! catalogue's availability, the create request's selection fields, the
//! `environmentSchema` projection, and the teardown record. Provisioning and
//! removal themselves are covered against a real worker in
//! `crates/worker/tests/provider_e2e.rs` and
//! `crates/worker/tests/worktree.rs`.

use std::time::Duration;

use loom_domain::{HostId, ProjectId, ProjectKind};
use loom_server::http::router;
use loom_server::state::{AppConfig, AppState};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const TIMEOUT: Duration = Duration::from_secs(10);

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

async fn request(addr: &str, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
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
    let status = head
        .split(' ')
        .nth(1)
        .and_then(|code| code.parse().ok())
        .expect("no HTTP status line");
    let body = serde_json::from_slice(&raw[separator..]).unwrap_or(Value::Null);
    (status, body)
}

async fn get(addr: &str, path: &str) -> (u16, Value) {
    request(addr, "GET", path, None).await
}

async fn post(addr: &str, path: &str, body: Value) -> (u16, Value) {
    request(addr, "POST", path, Some(body)).await
}

async fn delete(addr: &str, path: &str) -> (u16, Value) {
    request(addr, "DELETE", path, None).await
}

/// A connected host, a project, and optionally a checked-out source on it.
fn project_with_host(state: &AppState, with_source: bool) -> (HostId, ProjectId, Option<String>) {
    let now = loom_relay::now_ms();
    let (host, _) = state
        .registry
        .enroll_host(None, "worktree-host".into(), now)
        .unwrap();
    let (project, _) = state
        .registry
        .create_project("worktree".into(), ProjectKind::Standard, None, now)
        .unwrap();
    let path = with_source.then(|| "/srv/worktree-source".to_owned());
    if let Some(path) = &path {
        state
            .registry
            .add_project_source(&project.id, host.id.clone(), path.clone(), None, now)
            .unwrap();
    }
    (host.id, project.id, path)
}

#[tokio::test]
async fn the_worktree_provider_is_advertised_only_where_a_source_exists() {
    let (addr, state) = spawn_server().await;
    let (host_id, project_id, _) = project_with_host(&state, true);
    let (empty_host, empty_project_id, _) = project_with_host(&state, false);

    let (status, body) = get(
        &addr,
        &format!("/api/v1/system/environment-providers?projectId={project_id}&hostId={host_id}"),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let worktree = body["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|provider| provider["id"] == "git-worktree")
        .expect("the worktree provider is advertised");
    assert_eq!(worktree["requires"]["gitCheckout"], true);
    assert_eq!(worktree["requires"]["projectCheckout"], true);
    assert_eq!(worktree["acceptsEmptyInputs"], true);
    assert_eq!(worktree["availability"]["status"], "available");
    assert_eq!(
        worktree["machineAvailability"][host_id.to_string()]["status"],
        "available"
    );

    // The same provider is unavailable where the project has no source, and
    // for a host the project has no source on.
    let (status, body) = get(
        &addr,
        &format!(
            "/api/v1/system/environment-providers?projectId={empty_project_id}&hostId={empty_host}"
        ),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let worktree = body["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|provider| provider["id"] == "git-worktree")
        .unwrap();
    assert_eq!(worktree["availability"]["status"], "setup-required");
    assert_eq!(
        worktree["machineAvailability"][empty_host.to_string()]["status"],
        "unavailable"
    );

    let (status, body) = get(
        &addr,
        &format!("/api/v1/system/environment-providers?projectId={project_id}&hostId={empty_host}"),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let worktree = body["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|provider| provider["id"] == "git-worktree")
        .unwrap();
    assert_eq!(
        worktree["machineAvailability"][empty_host.to_string()]["status"],
        "unavailable"
    );

    state.shutdown().unwrap();
}

#[tokio::test]
async fn creating_a_worktree_environment_records_and_projects_its_selection() {
    let (addr, state) = spawn_server().await;
    let (host_id, project_id, _) = project_with_host(&state, true);

    let (status, created) = post(
        &addr,
        "/api/v1/environments",
        json!({
            "kind": "managed",
            "project_id": project_id,
            "host_id": host_id,
            "provider_id": "git-worktree",
            "base_branch": "origin/main",
        }),
    )
    .await;
    assert_eq!(status, 200, "{created}");
    let environment = &created["environment"];
    assert_eq!(environment["provider_id"], "git-worktree");
    assert_eq!(environment["base_branch"], "origin/main");
    let environment_id = environment["id"].as_str().unwrap().to_string();
    let branch = format!("loom/{environment_id}");
    assert_eq!(environment["branch_name"], branch);

    // The bb-shaped projection reports the worktree, not a personal workspace.
    let (status, projected) = get(&addr, &format!("/api/v1/environments/{environment_id}")).await;
    assert_eq!(status, 200, "{projected}");
    assert_eq!(projected["environmentProviderId"], "git-worktree");
    assert_eq!(projected["isWorktree"], true);
    assert_eq!(projected["workspaceProvisionType"], "managed-worktree");
    assert_eq!(projected["branchName"], branch);
    assert_eq!(projected["baseBranch"], "origin/main");
    assert_eq!(projected["mergeBaseBranch"], "origin/main");

    // The list projection carries the same identity.
    let (status, listed) = get(
        &addr,
        &format!("/api/v1/environments?project_id={project_id}"),
    )
    .await;
    assert_eq!(status, 200, "{listed}");
    let row = listed
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == environment_id)
        .unwrap();
    assert_eq!(row["environmentProviderId"], "git-worktree");
    assert_eq!(row["isWorktree"], true);

    state.shutdown().unwrap();
}

#[tokio::test]
async fn creating_a_worktree_without_a_source_is_refused() {
    let (addr, state) = spawn_server().await;
    let (host_id, project_id, _) = project_with_host(&state, false);

    let (status, body) = post(
        &addr,
        "/api/v1/environments",
        json!({
            "kind": "managed",
            "project_id": project_id,
            "host_id": host_id,
            "provider_id": "git-worktree",
        }),
    )
    .await;
    assert_eq!(status, 409, "{body}");
    assert!(
        body["message"]
            .as_str()
            .is_some_and(|error| error.contains("checked-out source")),
        "{body}"
    );
    assert!(state.registry.environments().is_empty());

    state.shutdown().unwrap();
}

#[tokio::test]
async fn destroying_a_managed_environment_projects_a_running_teardown() {
    let (addr, state) = spawn_server().await;
    let (host_id, project_id, _) = project_with_host(&state, true);

    let (_, created) = post(
        &addr,
        "/api/v1/environments",
        json!({
            "kind": "managed",
            "project_id": project_id,
            "host_id": host_id,
            "provider_id": "git-worktree",
        }),
    )
    .await;
    let environment_id = created["environment"]["id"].as_str().unwrap().to_string();

    let (status, destroyed) =
        delete(&addr, &format!("/api/v1/environments/{environment_id}")).await;
    assert_eq!(status, 200, "{destroyed}");
    assert_eq!(destroyed["ok"], true);

    let (status, projected) = get(&addr, &format!("/api/v1/environments/{environment_id}")).await;
    assert_eq!(status, 200, "{projected}");
    assert_eq!(projected["lifecycle"]["phase"], "teardown");
    assert_eq!(projected["lifecycle"]["teardown"]["status"], "running");
    assert_eq!(projected["lifecycle"]["teardown"]["attempt"], 1);
    // No worker is connected, so the removal request is still in the host
    // room: the projection is the honest "running", not "removed".
    assert!(projected["lifecycle"]["teardown"]["message"].is_null());

    state.shutdown().unwrap();
}

#[tokio::test]
async fn an_unmanaged_environment_keeps_its_kind_after_the_provider_field() {
    let (addr, state) = spawn_server().await;
    let (host_id, project_id, _) = project_with_host(&state, true);

    let (status, created) = post(
        &addr,
        "/api/v1/environments",
        json!({
            "kind": "unmanaged",
            "project_id": project_id,
            "host_id": host_id,
            "path": "/srv/checkout",
        }),
    )
    .await;
    assert_eq!(status, 200, "{created}");
    assert_eq!(created["environment"]["provider_id"], "project-checkout");
    let environment_id = created["environment"]["id"].as_str().unwrap().to_string();

    let (status, projected) = get(&addr, &format!("/api/v1/environments/{environment_id}")).await;
    assert_eq!(status, 200, "{projected}");
    assert_eq!(projected["isWorktree"], false);
    assert_eq!(projected["workspaceProvisionType"], "unmanaged");
    assert_eq!(projected["environmentProviderId"], "project-checkout");

    // Destroying an unmanaged environment is a record transition only: since
    // no teardown was dispatched, it stays settled as removed rather than
    // running.
    let (status, destroyed) =
        delete(&addr, &format!("/api/v1/environments/{environment_id}")).await;
    assert_eq!(status, 200, "{destroyed}");
    let (_, projected) = get(&addr, &format!("/api/v1/environments/{environment_id}")).await;
    assert_eq!(projected["lifecycle"]["phase"], "destroyed");
    assert!(projected["lifecycle"]["teardown"].is_null());

    state.shutdown().unwrap();
}

#[tokio::test]
async fn a_personal_managed_environment_is_not_projected_as_a_worktree() {
    let (addr, state) = spawn_server().await;
    let (host_id, project_id, _) = project_with_host(&state, true);

    let (status, created) = post(
        &addr,
        "/api/v1/environments",
        json!({
            "kind": "managed",
            "project_id": project_id,
            "host_id": host_id,
        }),
    )
    .await;
    assert_eq!(status, 200, "{created}");
    assert_eq!(created["environment"]["provider_id"], "personal-workspace");
    let environment_id = created["environment"]["id"].as_str().unwrap().to_string();

    let (status, projected) = get(&addr, &format!("/api/v1/environments/{environment_id}")).await;
    assert_eq!(status, 200, "{projected}");
    assert_eq!(projected["isWorktree"], false);
    assert_eq!(projected["workspaceProvisionType"], "personal");
    assert_eq!(projected["environmentProviderId"], "personal-workspace");

    state.shutdown().unwrap();
}
