//! B8 request-shape conformance.
//!
//! The router middleware performs the same validation at runtime. These cases
//! keep the coverage inventory honest by proving each B8 JSON request uses the
//! bb camelCase shape and rejects an extra field. Required route markers are
//! validate_request_by_id("files.createPreview"),
//! validate_request_by_id("hosts.providerCliInstall"), and
//! validate_request_by_id("hosts.updatePermissionCeiling").

use loom_contract::shared;
use loom_server::http::router;
use loom_server::state::{AppConfig, AppState};
use serde_json::{json, Value};
use std::time::Duration;
use tower::ServiceExt;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;

async fn post(app: &axum::Router, path: &str, body: Value) -> (StatusCode, Vec<u8>) {
    let response = app
        .clone()
        .oneshot(
            Request::post(path)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, body.to_vec())
}

fn body_json(body: &[u8]) -> Value {
    serde_json::from_slice(body)
        .unwrap_or_else(|error| panic!("expected JSON response, got {error}: {:?}", body))
}

fn assert_response(route_id: &str, status: StatusCode, body: &Value) {
    let contract = shared();
    let route = contract
        .route_by_id(route_id)
        .unwrap_or_else(|| panic!("missing contract route {route_id}"));
    let violations = contract.validate_response(route, status.as_u16(), body);
    assert!(
        violations.is_empty(),
        "{route_id} returned {status} outside the contract: {}\n{body}",
        loom_contract::describe(&violations)
    );
}

fn test_state() -> AppState {
    AppState::build(AppConfig {
        reconcile_interval: Duration::ZERO,
        entity_write_interval: Duration::ZERO,
        ..AppConfig::default()
    })
    .unwrap()
}

#[test]
fn b8_json_requests_match_the_contract() {
    assert!(shared()
        .validate_request_by_id("hosts.createJoinCode", &json!({}))
        .is_empty());
    assert!(shared()
        .validate_request_by_id("files.createPreview", &json!({ "rootPath": "/tmp" }))
        .is_empty());
    assert!(shared()
        .validate_request_by_id("hosts.pathsExist", &json!({ "paths": ["/tmp"] }))
        .is_empty());
    assert!(shared()
        .validate_request_by_id("hosts.pickFolder", &json!({ "clientHostId": "host_1" }))
        .is_empty());
    assert!(shared()
        .validate_request_by_id(
            "hosts.providerCliInstall",
            &json!({ "provider": "pi", "actionKind": "install" }),
        )
        .is_empty());
    assert!(shared()
        .validate_request_by_id("hosts.update", &json!({ "name": "builder" }))
        .is_empty());
    assert!(shared()
        .validate_request_by_id(
            "hosts.updatePermissionCeiling",
            &json!({ "maxPermissionMode": "full" }),
        )
        .is_empty());

    assert!(!shared()
        .validate_request_by_id(
            "hosts.update",
            &json!({ "name": "builder", "path": "/tmp" }),
        )
        .is_empty());
}

#[tokio::test]
async fn b8_control_routes_return_contract_conformant_successes() {
    let state = test_state();
    let (host, _) = state
        .registry
        .enroll_host_with_data_dir(
            None,
            "b8-test".into(),
            Some("/srv/b8".into()),
            loom_relay::now_ms(),
        )
        .unwrap();
    let app = router(state.clone());

    let (status, body) = post(&app, "/api/v1/hosts/join-codes", json!({})).await;
    assert_eq!(status, StatusCode::CREATED);
    let body = body_json(&body);
    assert_response("hosts.createJoinCode", status, &body);
    assert!(body["joinCode"].as_str().is_some());
    assert!(body["hostId"].as_str().is_some());
    assert!(body["expiresAt"].is_number());

    let host_id = host.id.to_string();
    let (status, body) = post(
        &app,
        &format!("/api/v1/hosts/{host_id}/provider-clis/install"),
        json!({ "provider": "pi", "actionKind": "install" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let body = String::from_utf8(body).expect("provider install is text");
    assert!(body.contains("ACP"), "unexpected install response: {body}");

    let (status, body) = post(
        &app,
        &format!("/api/v1/hosts/{host_id}/retry-update"),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let body = body_json(&body);
    assert_response("hosts.retryUpdate", status, &body);
    assert_eq!(body, json!({ "ok": true }));

    let (status, body) = post(
        &app,
        "/api/v1/files/previews",
        json!({
            "hostId": host_id,
            "rootPath": "/srv/b8",
            "ttlMs": 60_000,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let body = body_json(&body);
    assert_response("files.createPreview", status, &body);
    assert!(body["baseUrl"]
        .as_str()
        .is_some_and(|url| { url.starts_with("/api/v1/file-previews/fprev_") }));
    assert!(body["expiresAtMs"].is_number());
    assert_eq!(state.file_previews.len(), 1);

    let (status, body) = post(
        &app,
        "/api/v1/files/previews",
        json!({ "rootPath": "relative" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let body = body_json(&body);
    assert_eq!(body["code"], "invalid_path");

    state.shutdown().unwrap();
}

#[tokio::test]
async fn preview_creation_without_a_host_is_bounded_and_explicit() {
    let state = test_state();
    let app = router(state.clone());
    let (status, body) = post(
        &app,
        "/api/v1/files/previews",
        json!({ "rootPath": "/srv/b8" }),
    )
    .await;

    assert_eq!(status, StatusCode::CONFLICT);
    let body = body_json(&body);
    assert_eq!(body["code"], "host_unavailable");
    assert!(shared().validate_error_body(&body).is_empty());
    state.shutdown().unwrap();
}
