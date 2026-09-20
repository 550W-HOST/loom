//! B10 conformance and persistence tests.
//!
//! The settings surface is server-local state: successful mutations must be
//! contract-shaped, survive a durable restart, and publish typed cache
//! invalidations without exposing the settings payload through the relay.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use loom_contract::shared;
use loom_relay::Scope;
use loom_server::http::router;
use loom_server::state::{AppConfig, AppState};
use serde_json::{json, Value};
use tempfile::TempDir;
use tower::ServiceExt;

async fn body_json(response: axum::response::Response) -> Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes)
        .unwrap_or_else(|error| panic!("response was not JSON: {error}; body={:?}", bytes))
}

async fn body_bytes(response: axum::response::Response) -> Vec<u8> {
    response
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec()
}

async fn get(app: &Router, path: &str) -> axum::response::Response {
    app.clone()
        .oneshot(Request::get(path).body(Body::empty()).unwrap())
        .await
        .unwrap()
}

async fn put(app: &Router, path: &str, body: Value) -> axum::response::Response {
    app.clone()
        .oneshot(
            Request::put(path)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
}

async fn post_empty(app: &Router, path: &str) -> axum::response::Response {
    app.clone()
        .oneshot(Request::post(path).body(Body::empty()).unwrap())
        .await
        .unwrap()
}

async fn delete(app: &Router, path: &str) -> axum::response::Response {
    app.clone()
        .oneshot(Request::delete(path).body(Body::empty()).unwrap())
        .await
        .unwrap()
}

#[track_caller]
fn assert_response(route_id: &str, status: u16, body: &Value) {
    let contract = shared();
    let route = contract
        .route_by_id(route_id)
        .unwrap_or_else(|| panic!("missing contract route {route_id}"));
    let violations = contract.validate_response(route, status, body);
    assert!(
        violations.is_empty(),
        "{route_id} response is not contract-shaped: {violations:?}\n{body}"
    );
}

#[track_caller]
fn assert_error(status: StatusCode, body: &Value) {
    let contract = shared();
    assert!(
        contract.validate_error_body(body).is_empty(),
        "error body is not contract-shaped: {body}"
    );
    let code = body["code"].as_str().expect("error code");
    assert!(
        contract
            .error_statuses(code)
            .contains(&u64::from(status.as_u16())),
        "error code {code:?} is not declared at {}",
        status.as_u16()
    );
}

#[tokio::test]
async fn b10_routes_validate_requests_and_responses() {
    let state = AppState::build(AppConfig::default()).unwrap();
    let app = router(state.clone());
    let contract = shared();

    let appearance = json!({ "themeId": "default", "faviconColor": "blue" });
    assert!(contract
        .validate_request_by_id("system.appearance", &appearance)
        .is_empty());
    let response = put(&app, "/api/v1/settings/appearance", appearance).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_response("system.appearance", 200, &body);
    assert_eq!(body["faviconColor"], "blue");

    let experiments = json!({
        "changelogPreview": true,
        "mobileApp": false,
        "sidebarProgressiveDisclosure": true,
        "timelineWindowing": false
    });
    assert!(contract
        .validate_request_by_id("system.experiments", &experiments)
        .is_empty());
    let response = put(&app, "/api/v1/settings/experiments", experiments).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_response("system.experiments", 200, &body_json(response).await);

    let general = json!({
        "showKeyboardHints": false,
        "steerActiveThreadOnEnter": false,
        "showDiagnosticEvents": true,
        "providerOrder": ["pi"],
        "defaultProviderId": "pi",
        "streamerMode": true,
        "managedBranchPrefix": "loom/"
    });
    assert!(contract
        .validate_request_by_id("system.generalSettings", &general)
        .is_empty());
    let response = put(&app, "/api/v1/settings/general", general).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_response("system.generalSettings", 200, &body_json(response).await);

    let keyboard = json!([]);
    assert!(contract
        .validate_request_by_id("system.keyboardSettings", &keyboard)
        .is_empty());
    let response = put(&app, "/api/v1/settings/keyboard", keyboard).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_response("system.keyboardSettings", 200, &body_json(response).await);

    let response = get(&app, "/api/v1/settings/themes").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_response("system.themes", 200, &body_json(response).await);
    let response = get(&app, "/api/v1/settings/themes/default").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_response("system.resolveTheme", 200, &body_json(response).await);

    let response = get(&app, "/api/v1/preferences/ui").await;
    assert_eq!(response.status(), StatusCode::OK);
    let preferences = body_json(response).await;
    assert_response("system.uiPreferences", 200, &preferences);
    assert_eq!(
        preferences["preferences"]["sidebar.sortDirection"]["revision"],
        0
    );

    let update = json!({ "expectedRevision": 0, "value": "ascending" });
    assert!(contract
        .validate_request_by_id("system.updateUiPreference", &update)
        .is_empty());
    let response = put(&app, "/api/v1/preferences/ui/sidebar.sortDirection", update).await;
    assert_eq!(response.status(), StatusCode::OK);
    let updated = body_json(response).await;
    assert_response("system.updateUiPreference", 200, &updated);
    assert_eq!(updated["revision"], 1);

    let stale = put(
        &app,
        "/api/v1/preferences/ui/sidebar.sortDirection",
        json!({ "expectedRevision": 0, "value": "descending" }),
    )
    .await;
    assert_eq!(stale.status(), StatusCode::CONFLICT);
    let stale_body = body_json(stale).await;
    assert_error(StatusCode::CONFLICT, &stale_body);

    let response = delete(&app, "/api/v1/preferences/ui/sidebar.sortDirection").await;
    assert_eq!(response.status(), StatusCode::OK);
    let reset = body_json(response).await;
    assert_response("system.resetUiPreference", 200, &reset);
    assert_eq!(reset["value"], "default");

    let response = post_empty(&app, "/api/v1/system/config/reload").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_response("system.reloadConfig", 200, &body_json(response).await);

    let response = get(&app, "/api/v1/system/usage-limits").await;
    assert_eq!(response.status(), StatusCode::OK);
    let limits = body_json(response).await;
    assert_response("system.usageLimits", 200, &limits);
    assert_eq!(limits["pi"]["status"], "error");

    // The provider mark is the glyph loom ships — bb's drawn icon, painted with
    // `currentColor` so the client's mask gives it the theme's own ink — and the
    // URL the API hands out is content-addressed, which is what makes it
    // cacheable forever.
    let advertised = body_json(get(&app, "/api/v1/system/providers").await).await;
    let pi = advertised
        .as_array()
        .expect("providers is a list")
        .iter()
        .find(|provider| provider["id"] == "pi")
        .expect("pi is offered");
    assert_eq!(
        pi["strings"]["signInHint"],
        "Run `pi` on the machine to sign in."
    );
    assert_eq!(pi["strings"]["iconTint"]["light"], "#6D5DFB");
    assert_eq!(pi["strings"]["iconTint"]["dark"], "#6D5DFB");
    let logo_url = pi["logoUrl"]
        .as_str()
        .expect("pi advertises a logo")
        .to_owned();
    assert!(
        logo_url.starts_with("/api/v1/system/providers/pi/logo?h="),
        "a mark is addressed by content: {logo_url}"
    );

    let logo = get(&app, "/api/v1/system/providers/pi/logo").await;
    assert_eq!(logo.status(), StatusCode::OK);
    assert_eq!(logo.headers()["content-type"], "image/svg+xml");
    assert_eq!(logo.headers()["cache-control"], "no-store");
    let logo_body = body_bytes(logo).await;
    assert!(
        logo_body.starts_with(b"<svg") && logo_body.ends_with(b"</svg>\n"),
        "the provider logo is loom's own mark: {:?}",
        String::from_utf8_lossy(&logo_body)
    );
    assert!(
        String::from_utf8_lossy(&logo_body).contains("currentColor"),
        "a mark that does not follow the theme would vanish in one of them"
    );

    let hashed = get(&app, &logo_url).await;
    assert_eq!(hashed.status(), StatusCode::OK);
    assert_eq!(
        hashed.headers()["cache-control"],
        "public, max-age=31536000, immutable"
    );
    assert_eq!(body_bytes(hashed).await, logo_body);

    let stale = get(&app, "/api/v1/system/providers/pi/logo?h=0000000000000000").await;
    assert_eq!(stale.status(), StatusCode::OK);
    assert_eq!(stale.headers()["cache-control"], "no-store");

    let unknown_logo = get(&app, "/api/v1/system/providers/nope/logo").await;
    assert_eq!(unknown_logo.status(), StatusCode::NOT_FOUND);
    assert_error(StatusCode::NOT_FOUND, &body_json(unknown_logo).await);

    let unknown_theme = get(&app, "/api/v1/settings/themes/no-such-theme").await;
    assert_eq!(unknown_theme.status(), StatusCode::NOT_FOUND);
    assert_error(StatusCode::NOT_FOUND, &body_json(unknown_theme).await);

    let changes = state
        .relay
        .replay_scope(&Scope::Global, 20)
        .unwrap()
        .into_iter()
        .filter_map(|envelope| {
            let frame: Value = serde_json::from_slice(&envelope.payload).ok()?;
            serde_json::from_str::<Value>(frame["payload"].as_str()?).ok()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        changes,
        vec![
            json!({ "type": "changed", "entity": "system", "changes": ["config-changed"] }),
            json!({ "type": "changed", "entity": "system", "changes": ["config-changed"] }),
            json!({ "type": "changed", "entity": "system", "changes": ["config-changed"] }),
            json!({ "type": "changed", "entity": "system", "changes": ["config-changed"] }),
            json!({ "type": "changed", "entity": "system", "changes": ["ui-preferences-changed"] }),
            json!({ "type": "changed", "entity": "system", "changes": ["ui-preferences-changed"] }),
        ]
    );
    state.shutdown().unwrap();
}

#[tokio::test]
async fn b10_settings_survive_a_durable_server_restart() {
    let dir = TempDir::new().unwrap();
    let config = AppConfig {
        backend_path: Some(dir.path().to_path_buf()),
        reconcile_interval: std::time::Duration::ZERO,
        entity_write_interval: std::time::Duration::ZERO,
        ..AppConfig::default()
    };
    let state = AppState::build(config.clone()).unwrap();
    let app = router(state.clone());

    let response = put(
        &app,
        "/api/v1/settings/appearance",
        json!({ "themeId": "default", "faviconColor": "teal" }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let response = put(
        &app,
        "/api/v1/preferences/ui/sidebar.organizationMode",
        json!({ "expectedRevision": 0, "value": "machine" }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let response = put(
        &app,
        "/api/v1/settings/general",
        json!({
            "showKeyboardHints": true,
            "steerActiveThreadOnEnter": true,
            "showDiagnosticEvents": false,
            "providerOrder": [],
            "defaultProviderId": null,
            "streamerMode": false,
            "managedBranchPrefix": ""
        }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    state.shutdown().unwrap();

    let restored = AppState::build(config).unwrap();
    let app = router(restored.clone());
    let appearance = body_json(get(&app, "/api/v1/settings/themes").await).await;
    assert_eq!(appearance["active"]["faviconColor"], "teal");
    let preferences = body_json(get(&app, "/api/v1/preferences/ui").await).await;
    assert_eq!(
        preferences["preferences"]["sidebar.organizationMode"]["value"],
        "machine"
    );
    assert_eq!(
        preferences["preferences"]["sidebar.organizationMode"]["revision"],
        1
    );
    let config = body_json(get(&app, "/api/v1/system/config").await).await;
    assert_eq!(config["generalSettings"]["providerOrder"], json!([]));
    assert!(config["generalSettings"]["defaultProviderId"].is_null());
    restored.shutdown().unwrap();
}

#[tokio::test]
async fn b10_voice_transcription_reports_missing_capability() {
    let state = AppState::build(AppConfig::default()).unwrap();
    let app = router(state.clone());
    let response = app
        .oneshot(
            Request::post("/api/v1/system/voice-transcription")
                .header("content-type", "multipart/form-data; boundary=b10-boundary")
                .body(Body::from("--b10-boundary--\r\n".as_bytes().to_vec()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
    assert_error(StatusCode::NOT_IMPLEMENTED, &body_json(response).await);
    state.shutdown().unwrap();
}
