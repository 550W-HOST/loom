//! Automations conformance and persistence tests.
//!
//! Automations are loom-native: bb's contract has no route for them, so the
//! schemas these tests validate against are loom's own, mirroring
//! `ui/packages/automations/src/rpc-types.ts` — the pinned contract the UI seam
//! was imported against. Every request body is validated against its schema
//! before it is sent and every response body after it is read, with the same
//! validator the bb contract routes use (`loom_contract::validate`).

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use loom_domain::EnvironmentId;
use loom_server::automations::{AutomationState, StoredAutomation, StoredAutomationRun};
use loom_server::http::router;
use loom_server::state::{AppConfig, AppState};
use serde_json::{json, Value};
use tempfile::TempDir;
use tower::ServiceExt;

/* ------------------------------------------------------------------ */
/* Contract shapes (loom-authored, from rpc-types.ts)                  */
/* ------------------------------------------------------------------ */

use loom_server::automations_contract as contract;

/* ------------------------------------------------------------------ */
/* Helpers                                                             */
/* ------------------------------------------------------------------ */

/// Validates an instance against a loom-authored automation schema.
///
/// The schemas declare no `$ref`, so the document root is irrelevant; the
/// validator itself is the contract one.
#[track_caller]
fn assert_schema(schema: &Value, instance: &Value, what: &str) {
    let violations = contract::validate_response(schema, instance);
    assert!(
        violations.is_empty(),
        "{what} is not contract-shaped: {violations:?}\n{instance}"
    );
}

async fn body_json(response: axum::response::Response) -> Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes)
        .unwrap_or_else(|error| panic!("response was not JSON: {error}; body={bytes:?}"))
}

async fn request(
    app: &Router,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> axum::response::Response {
    let builder = Request::builder().method(method).uri(path);
    let request = match body {
        Some(body) => builder
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    };
    app.clone().oneshot(request).await.unwrap()
}

async fn get(app: &Router, path: &str) -> axum::response::Response {
    request(app, "GET", path, None).await
}

async fn post(app: &Router, path: &str, body: Option<Value>) -> axum::response::Response {
    request(app, "POST", path, body).await
}

async fn patch(app: &Router, path: &str, body: Value) -> axum::response::Response {
    request(app, "PATCH", path, Some(body)).await
}

async fn delete(app: &Router, path: &str) -> axum::response::Response {
    request(app, "DELETE", path, None).await
}

/// An agent execution body, in the contract's shape.
fn agent_execution(environment: &str) -> Value {
    json!({
        "mode": "agent",
        "prompt": "summarise the repository",
        "providerId": "pi",
        "model": "pi/default",
        "reasoningLevel": "medium",
        "permissionMode": "auto",
        "environment": { "type": "reuse", "environmentId": environment }
    })
}

/// A create body, in the contract's shape.
///
/// The execution reuses an environment that exists and is ready. An automation
/// whose environment cannot be resolved is a *run* failure, not a create
/// failure, and these tests are about the routes: the fixture hands the
/// executor something it can actually dispatch to.
fn create_body(name: &str, environment: &EnvironmentId) -> Value {
    let body = json!({
        "name": name,
        "trigger": { "triggerType": "schedule", "cron": "0 9 * * 1-5", "timezone": "Europe/Paris" },
        "execution": agent_execution(&environment.to_string()),
        "origin": "human"
    });
    assert_schema(&contract::create_request(), &body, "create request");
    body
}

/// An enrolled host with a ready workspace, and the environment bound to it.
///
/// A schedule that fires has to run *somewhere*: the fixture provides one
/// connected machine and one unmanaged environment, which is the smallest
/// deployment an automation can execute in.
fn executable_environment(state: &AppState) -> EnvironmentId {
    let (host, host_events) = state
        .registry
        .enroll_host(None, "worker".into(), 1)
        .expect("enrolls a host");
    for event in &host_events {
        state.publish_domain_event(event).expect("publishes");
    }
    let (environment, events) = state
        .registry
        .create_environment(
            Some(state.registry.personal_project_id()),
            host.id,
            loom_domain::EnvironmentKind::Unmanaged,
            Some("/srv/loom".into()),
            1,
        )
        .expect("creates an environment");
    for event in &events {
        state.publish_domain_event(event).expect("publishes");
    }
    environment.id
}

/// The ephemeral server the route tests run against.
async fn server() -> (AppState, Router, String, EnvironmentId) {
    let state = AppState::build(AppConfig {
        reconcile_interval: std::time::Duration::ZERO,
        schedule_interval: std::time::Duration::ZERO,
        ..AppConfig::default()
    })
    .unwrap();
    let app = router(state.clone());
    let project = state.registry.personal_project_id().to_string();
    let environment = executable_environment(&state);
    (state, app, project, environment)
}

/* ------------------------------------------------------------------ */
/* Tests                                                              */
/* ------------------------------------------------------------------ */

#[tokio::test]
async fn automations_crud_overview_and_history_are_contract_shaped() {
    let (state, app, project, environment) = server().await;

    // create
    let response = post(
        &app,
        &format!("/api/v1/projects/{project}/automations"),
        Some(create_body("nightly", &environment)),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let created = body_json(response).await;
    assert_schema(&contract::response(), &created, "create response");
    assert_eq!(created["name"], "nightly");
    assert_eq!(created["enabled"], true);
    assert_eq!(created["origin"], "human");
    assert_eq!(created["runCount"], 0);
    assert!(created["lastRunAt"].is_null());
    assert!(created["lastRunStatus"].is_null());
    // An enabled schedule reports when it is next due, in its zone.
    assert!(created["nextRunAt"].as_u64().is_some_and(|next| next > 0));
    let automation = created["id"].as_str().unwrap().to_owned();

    // get
    let response = get(
        &app,
        &format!("/api/v1/projects/{project}/automations/{automation}"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let fetched = body_json(response).await;
    assert_schema(&contract::read_result(), &fetched, "get response");
    assert_eq!(fetched, created);

    // list
    let response = get(&app, &format!("/api/v1/projects/{project}/automations")).await;
    assert_eq!(response.status(), StatusCode::OK);
    let listed = body_json(response).await;
    assert_schema(
        &json!({ "type": "array", "items": contract::read_result() }),
        &listed,
        "list response",
    );
    assert_eq!(listed.as_array().unwrap().len(), 1);

    // overview names the owning project
    let response = get(&app, "/api/v1/automations").await;
    assert_eq!(response.status(), StatusCode::OK);
    let overview = body_json(response).await;
    assert_schema(
        &json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["automations"],
            "properties": {
                "automations": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "additionalProperties": false,
                        "required": ["automation", "project"],
                        "properties": {
                            "automation": contract::read_result(),
                            "project": {
                                "type": "object",
                                "additionalProperties": false,
                                "required": ["id", "name"],
                                "properties": { "id": { "type": "string" }, "name": { "type": "string" } }
                            }
                        }
                    }
                }
            }
        }),
        &overview,
        "overview response",
    );
    assert_eq!(overview["automations"][0]["automation"], created);
    assert_eq!(overview["automations"][0]["project"]["id"], project);
    assert_eq!(overview["automations"][0]["project"]["name"], "Personal");

    // update: a name change and an agent patch merge
    let body = json!({ "name": "morning", "agent": { "model": "pi/faster", "serviceTier": null } });
    assert_schema(&contract::update_request(), &body, "update request");
    let response = patch(
        &app,
        &format!("/api/v1/projects/{project}/automations/{automation}"),
        body,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let updated = body_json(response).await;
    assert_schema(&contract::response(), &updated, "update response");
    assert_eq!(updated["name"], "morning");
    assert_eq!(updated["execution"]["model"], "pi/faster");
    assert_eq!(updated["execution"]["prompt"], "summarise the repository");
    assert_eq!(updated["execution"]["reasoningLevel"], "medium");
    assert!(updated["updatedAt"].as_u64().unwrap() >= created["updatedAt"].as_u64().unwrap());

    // pause and resume
    let response = post(
        &app,
        &format!("/api/v1/projects/{project}/automations/{automation}/pause"),
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let paused = body_json(response).await;
    assert_schema(&contract::response(), &paused, "pause response");
    assert_eq!(paused["enabled"], false);
    assert!(paused["nextRunAt"].is_null());

    let response = post(
        &app,
        &format!("/api/v1/projects/{project}/automations/{automation}/resume"),
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let resumed = body_json(response).await;
    assert_schema(&contract::response(), &resumed, "resume response");
    assert_eq!(resumed["enabled"], true);

    // run: the manual trigger records a run and returns it
    let body = json!({ "idempotencyKey": "manual-1" });
    assert_schema(&contract::run_request(), &body, "run request");
    let response = post(
        &app,
        &format!("/api/v1/projects/{project}/automations/{automation}/run"),
        Some(body.clone()),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let run = body_json(response).await;
    assert_schema(
        &json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["run"],
            "properties": { "run": contract::run_response() }
        }),
        &run,
        "run response",
    );
    assert_eq!(run["run"]["status"], "running");
    assert_eq!(run["run"]["trigger"], "manual");
    assert_eq!(run["run"]["runMode"], "agent");
    // The run was dispatched: it names the thread the turn runs in, and that
    // thread is the one the provider is working in.
    let run_thread = run["run"]["threadId"]
        .as_str()
        .expect("a dispatched run names its thread")
        .to_owned();
    assert_eq!(
        state
            .registry
            .thread(
                &run_thread
                    .parse::<loom_domain::ThreadId>()
                    .expect("a thread id")
            )
            .expect("the thread exists")
            .status,
        loom_domain::ThreadStatus::Working
    );
    assert!(run["run"]["finishedAt"].is_null());
    assert!(run["run"].get("idempotencyKey").is_none());
    let run_id = run["run"]["id"].as_str().unwrap().to_owned();

    // the same key returns the same run instead of starting another, and says
    // so with a 200 rather than a 201
    let response = post(
        &app,
        &format!("/api/v1/projects/{project}/automations/{automation}/run"),
        Some(json!({ "idempotencyKey": "manual-1" })),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_json(response).await["run"]["id"], json!(run_id));

    // a different key does not start a second run while one is in flight
    let response = post(
        &app,
        &format!("/api/v1/projects/{project}/automations/{automation}/run"),
        Some(json!({ "idempotencyKey": "manual-2" })),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_json(response).await["run"]["id"], json!(run_id));

    // runs: the history lists it, newest first, with a null cursor at the end
    let response = get(
        &app,
        &format!("/api/v1/projects/{project}/automations/{automation}/runs"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let runs = body_json(response).await;
    assert_schema(
        &json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["runs", "nextCursor"],
            "properties": {
                "runs": { "type": "array", "items": contract::run_response() },
                "nextCursor": { "type": ["string", "null"] }
            }
        }),
        &runs,
        "runs response",
    );
    assert_eq!(runs["runs"].as_array().unwrap().len(), 1);
    assert_eq!(runs["runs"][0]["id"], json!(run_id));
    assert!(runs["nextCursor"].is_null());

    // delete
    let response = delete(
        &app,
        &format!("/api/v1/projects/{project}/automations/{automation}"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let deleted = body_json(response).await;
    assert_schema(
        &json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["ok"],
            "properties": { "ok": { "const": true } }
        }),
        &deleted,
        "delete response",
    );
    let response = get(
        &app,
        &format!("/api/v1/projects/{project}/automations/{automation}"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert!(
        body_json(get(&app, "/api/v1/automations").await).await["automations"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    state.shutdown();
}

#[tokio::test]
async fn a_once_trigger_arms_its_instant_and_a_past_one_is_refused() {
    let (state, app, project, environment) = server().await;
    let future = 4_000_000_000_000u64;

    let mut body = create_body("one shot", &environment);
    body["trigger"] = json!({ "triggerType": "once", "runAt": future });
    assert_schema(&contract::create_request(), &body, "create request");
    let response = post(
        &app,
        &format!("/api/v1/projects/{project}/automations"),
        Some(body),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let created = body_json(response).await;
    assert_schema(&contract::response(), &created, "create response");
    assert_eq!(created["nextRunAt"], future);

    let mut past = create_body("too late", &environment);
    past["trigger"] = json!({ "triggerType": "once", "runAt": 1 });
    assert_schema(&contract::create_request(), &past, "create request");
    let response = post(
        &app,
        &format!("/api/v1/projects/{project}/automations"),
        Some(past),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let error = body_json(response).await;
    assert_eq!(error["code"], "invalid_request");
    assert!(error["message"].as_str().unwrap().contains("trigger.runAt"));

    // An inline script needs exactly one source, and a bad cron five fields.
    let mut both_script_sources = create_body("scripted", &environment);
    both_script_sources["execution"] = json!({
        "mode": "script",
        "script": "echo hi",
        "scriptFile": "/srv/job.sh",
        "interpreter": "bash",
        "timeoutMs": 120000
    });
    assert_schema(
        &contract::create_request(),
        &both_script_sources,
        "create request",
    );
    let response = post(
        &app,
        &format!("/api/v1/projects/{project}/automations"),
        Some(both_script_sources),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body_json(response).await["code"], "invalid_request");

    let mut bad_cron = create_body("bad cron", &environment);
    bad_cron["trigger"] =
        json!({ "triggerType": "schedule", "cron": "0 9 * *", "timezone": "UTC" });
    let response = post(
        &app,
        &format!("/api/v1/projects/{project}/automations"),
        Some(bad_cron),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let error = body_json(response).await;
    assert!(error["message"].as_str().unwrap().contains("trigger.cron"));

    let mut bad_timezone = create_body("bad timezone", &environment);
    bad_timezone["trigger"] =
        json!({ "triggerType": "schedule", "cron": "0 9 * * *", "timezone": "Europe Paris" });
    let response = post(
        &app,
        &format!("/api/v1/projects/{project}/automations"),
        Some(bad_timezone),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let error = body_json(response).await;
    assert!(error["message"]
        .as_str()
        .unwrap()
        .contains("trigger.timezone"));
    state.shutdown();
}

#[tokio::test]
async fn automation_requests_are_strict_and_unknown_targets_are_refused() {
    let (state, app, project, environment) = server().await;

    // An unknown field is not silently dropped. Rejecting a body that does not
    // match the request shape is the framework's job, so it is a 422 in the
    // same uniform error shape the contract middleware uses.
    let mut unknown_field = create_body("strict", &environment);
    unknown_field["unexpected"] = json!(true);
    let response = post(
        &app,
        &format!("/api/v1/projects/{project}/automations"),
        Some(unknown_field),
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body_json(response).await["code"], "invalid_request");

    // An unknown field inside a union member is refused as well.
    let mut unknown_inner = create_body("strict inner", &environment);
    unknown_inner["execution"]["unexpected"] = json!(true);
    let response = post(
        &app,
        &format!("/api/v1/projects/{project}/automations"),
        Some(unknown_inner),
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);

    // An update with nothing to change, or with both executions, is refused.
    let automation = body_json(
        post(
            &app,
            &format!("/api/v1/projects/{project}/automations"),
            Some(create_body("targets", &environment)),
        )
        .await,
    )
    .await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let path = format!("/api/v1/projects/{project}/automations/{automation}");
    let response = patch(&app, &path, json!({})).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(body_json(response).await["message"]
        .as_str()
        .unwrap()
        .contains("at least one field"));
    let response = patch(
        &app,
        &path,
        json!({ "execution": agent_execution(&environment.to_string()), "agent": { "model": "pi/other" } }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(body_json(response).await["message"]
        .as_str()
        .unwrap()
        .contains("cannot be combined"));

    // Unknown project, unknown automation and a malformed id each answer for
    // themselves: 404 for what is missing, 400 for what is not an id at all.
    let response = get(
        &app,
        &format!(
            "/api/v1/projects/{}/automations",
            "proj_00000000000000000000000000"
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(body_json(response).await["code"], "not_found");
    let response = get(&app, "/api/v1/projects/not-an-id/automations").await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let response = get(
        &app,
        &format!("/api/v1/projects/{project}/automations/not-an-id"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let response = get(
        &app,
        &format!(
            "/api/v1/projects/{project}/automations/{}",
            "auto_00000000000000000000000000"
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    // A malformed run cursor and an out-of-range page size are request errors.
    let runs_path = format!("/api/v1/projects/{project}/automations/{automation}/runs");
    let response = get(&app, &format!("{runs_path}?cursor=!!!!")).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let response = get(&app, &format!("{runs_path}?limit=0")).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let response = get(&app, &format!("{runs_path}?limit=201")).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    state.shutdown();
}

#[tokio::test]
async fn automations_are_scoped_to_their_project() {
    let (state, app, project, environment) = server().await;
    let automation = body_json(
        post(
            &app,
            &format!("/api/v1/projects/{project}/automations"),
            Some(create_body("scoped", &environment)),
        )
        .await,
    )
    .await["id"]
        .as_str()
        .unwrap()
        .to_owned();

    // A second project, created through the registry the same way the project
    // routes do: the workspace's own host is not enrolled in this fixture.
    let (other, _) = state
        .registry
        .create_project(
            "Other".to_owned(),
            loom_domain::ProjectKind::Standard,
            None,
            1,
        )
        .expect("the second project is created");
    let other = other.id.to_string();
    assert_ne!(other, project);

    // The automation is not visible, nor findable, from the other project.
    let response = get(&app, &format!("/api/v1/projects/{other}/automations")).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(body_json(response).await.as_array().unwrap().is_empty());
    let response = get(
        &app,
        &format!("/api/v1/projects/{other}/automations/{automation}"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let response = delete(
        &app,
        &format!("/api/v1/projects/{other}/automations/{automation}"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    // …while it is still there for its own project.
    let response = get(
        &app,
        &format!("/api/v1/projects/{project}/automations/{automation}"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    state.shutdown();
}

#[tokio::test]
async fn the_write_routes_refuse_a_key_the_contract_does_not_name() {
    let (state, app, project, environment) = server().await;
    let automation = body_json(
        post(
            &app,
            &format!("/api/v1/projects/{project}/automations"),
            Some(create_body("strict", &environment)),
        )
        .await,
    )
    .await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let create_path = format!("/api/v1/projects/{project}/automations");
    let automation_path = format!("/api/v1/projects/{project}/automations/{automation}");
    let run_path = format!("{automation_path}/run");

    // Every shape the contract spells `.strict()` rejects an extra key — the
    // unions nested inside a body included, which is what serde cannot express
    // and why the `automations_contract` schemas run on the way in.
    let cases: Vec<(&str, &str, String, Value)> = vec![
        (
            "create",
            "top level",
            create_path.clone(),
            json!({ "name": "strict", "trigger": { "triggerType": "schedule", "cron": "0 9 * * *", "timezone": "UTC" }, "execution": agent_execution(&environment.to_string()), "origin": "human", "extra": 1 }),
        ),
        (
            "create",
            "schedule trigger",
            create_path.clone(),
            json!({ "name": "strict", "trigger": { "triggerType": "schedule", "cron": "0 9 * * *", "timezone": "UTC", "extra": 1 }, "execution": agent_execution(&environment.to_string()), "origin": "human" }),
        ),
        (
            "create",
            "once trigger",
            create_path.clone(),
            json!({ "name": "strict", "trigger": { "triggerType": "once", "runAt": 4_000_000_000_000u64, "extra": 1 }, "execution": agent_execution(&environment.to_string()), "origin": "human" }),
        ),
        (
            "create",
            "project-default environment",
            create_path.clone(),
            json!({ "name": "strict", "trigger": { "triggerType": "schedule", "cron": "0 9 * * *", "timezone": "UTC" }, "execution": { "mode": "agent", "prompt": "p", "providerId": "pi", "model": "m", "reasoningLevel": "medium", "permissionMode": "auto", "environment": { "type": "project-default", "extra": 1 } }, "origin": "human" }),
        ),
        (
            "create",
            "host environment",
            create_path.clone(),
            json!({ "name": "strict", "trigger": { "triggerType": "schedule", "cron": "0 9 * * *", "timezone": "UTC" }, "execution": { "mode": "agent", "prompt": "p", "providerId": "pi", "model": "m", "reasoningLevel": "medium", "permissionMode": "auto", "environment": { "type": "host", "hostId": "host_00000000000000000000000000", "workspace": { "type": "personal" }, "extra": 1 } }, "origin": "human" }),
        ),
        (
            "create",
            "unmanaged workspace",
            create_path.clone(),
            json!({ "name": "strict", "trigger": { "triggerType": "schedule", "cron": "0 9 * * *", "timezone": "UTC" }, "execution": { "mode": "agent", "prompt": "p", "providerId": "pi", "model": "m", "reasoningLevel": "medium", "permissionMode": "auto", "environment": { "type": "host", "hostId": "host_00000000000000000000000000", "workspace": { "type": "unmanaged", "path": null, "extra": 1 } } }, "origin": "human" }),
        ),
        (
            "create",
            "workspace branch",
            create_path.clone(),
            json!({ "name": "strict", "trigger": { "triggerType": "schedule", "cron": "0 9 * * *", "timezone": "UTC" }, "execution": { "mode": "agent", "prompt": "p", "providerId": "pi", "model": "m", "reasoningLevel": "medium", "permissionMode": "auto", "environment": { "type": "host", "hostId": "host_00000000000000000000000000", "workspace": { "type": "unmanaged", "path": null, "branch": { "kind": "existing", "name": "main", "extra": 1 } } } }, "origin": "human" }),
        ),
        (
            "update",
            "agent target",
            automation_path.clone(),
            json!({ "agent": { "target": { "type": "environment", "environment": { "type": "project-default" }, "extra": 1 } } }),
        ),
        (
            "run",
            "top level",
            run_path.clone(),
            json!({ "idempotencyKey": "key", "extra": 1 }),
        ),
    ];

    for (operation, label, path, body) in cases {
        let response = match operation {
            "create" => post(&app, &path, Some(body)).await,
            "update" => patch(&app, &path, body).await,
            _ => post(&app, &path, Some(body)).await,
        };
        assert_eq!(
            response.status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "{operation}: {label} was accepted"
        );
        let error = body_json(response).await;
        assert_eq!(error["code"], "invalid_request");
        assert!(
            error["message"]
                .as_str()
                .is_some_and(|message| message.contains("automations contract")),
            "{operation}: {label} answered with {error}"
        );
    }

    // The same shapes without the extra key are accepted, so the rejection is
    // the extra key and not the shape.
    let mut valid = create_body("strict again", &environment);
    valid["trigger"] = json!({ "triggerType": "once", "runAt": 4_000_000_000_000u64 });
    let response = post(&app, &create_path, Some(valid)).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let created = body_json(response).await;
    assert_schema(&contract::response(), &created, "create response");
    state.shutdown();
}

#[tokio::test]
async fn automations_survive_a_durable_server_restart() {
    let dir = TempDir::new().unwrap();
    let config = AppConfig {
        backend_path: Some(dir.path().to_path_buf()),
        reconcile_interval: std::time::Duration::ZERO,
        snapshot_interval: std::time::Duration::ZERO,
        ..AppConfig::default()
    };
    let state = AppState::build(config.clone()).unwrap();
    let app = router(state.clone());
    let project = state.registry.personal_project_id().to_string();
    let environment = executable_environment(&state);

    let created = body_json(
        post(
            &app,
            &format!("/api/v1/projects/{project}/automations"),
            Some(create_body("durable", &environment)),
        )
        .await,
    )
    .await;
    let automation = created["id"].as_str().unwrap().to_owned();
    let run = body_json(
        post(
            &app,
            &format!("/api/v1/projects/{project}/automations/{automation}/run"),
            Some(json!({ "idempotencyKey": "before-restart" })),
        )
        .await,
    )
    .await;
    let run_id = run["run"]["id"].as_str().unwrap().to_owned();
    state.shutdown();

    let restored = AppState::build(config).unwrap();
    let app = router(restored.clone());
    let response = get(
        &app,
        &format!("/api/v1/projects/{project}/automations/{automation}"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let fetched = body_json(response).await;
    assert_schema(&contract::read_result(), &fetched, "restored get response");
    // The automation itself is unchanged — same id, name, trigger, execution
    // and schedule — and the run it dispatched is what the restart settled: a
    // turn nobody can prove was still running is failed, and the automation
    // records that failure rather than pretending the run is still in flight.
    assert_eq!(fetched["id"], created["id"]);
    assert_eq!(fetched["name"], created["name"]);
    assert_eq!(fetched["trigger"], created["trigger"]);
    assert_eq!(fetched["execution"], created["execution"]);
    assert_eq!(fetched["nextRunAt"], created["nextRunAt"]);
    assert_eq!(fetched["enabled"], created["enabled"]);
    assert_eq!(fetched["lastRunStatus"], "failed");
    assert_eq!(
        fetched["lastError"],
        "the server restarted while this run was in flight"
    );
    assert_eq!(
        fetched["runCount"], 0,
        "a manual run is not a scheduled window"
    );

    let runs = body_json(
        get(
            &app,
            &format!("/api/v1/projects/{project}/automations/{automation}/runs"),
        )
        .await,
    )
    .await;
    assert_schema(
        &json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["runs", "nextCursor"],
            "properties": {
                "runs": { "type": "array", "items": contract::run_response() },
                "nextCursor": { "type": ["string", "null"] }
            }
        }),
        &runs,
        "restored runs response",
    );
    assert_eq!(runs["runs"][0]["id"], json!(run_id));
    assert_eq!(
        runs["runs"][0]["status"], "failed",
        "the interrupted turn was failed once, by the restart"
    );
    assert!(runs["runs"][0]["threadId"].is_string());
    assert_eq!(
        body_json(get(&app, "/api/v1/automations").await).await["automations"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    restored.shutdown();
}

#[tokio::test]
async fn an_older_snapshot_without_automations_still_loads() {
    let dir = TempDir::new().unwrap();
    let config = AppConfig {
        backend_path: Some(dir.path().to_path_buf()),
        reconcile_interval: std::time::Duration::ZERO,
        snapshot_interval: std::time::Duration::ZERO,
        ..AppConfig::default()
    };
    let state = AppState::build(config.clone()).unwrap();
    let project = state.registry.personal_project_id().to_string();
    let environment = executable_environment(&state);
    let app = router(state.clone());
    post(
        &app,
        &format!("/api/v1/projects/{project}/automations"),
        Some(create_body("kept", &environment)),
    )
    .await;
    // Shut down first, so the final snapshot cannot overwrite the hand-edited
    // one this test restores from.
    state.shutdown();

    // A snapshot written before automations existed: the field is absent
    // entirely, which is exactly what `#[serde(default)]` is for.
    let path = dir.path().join("domain.snapshot");
    let bytes = std::fs::read(&path).unwrap();
    let header_len = 8 + 4 + 8 + 4;
    let payload = &bytes[header_len..];
    let mut snapshot: Value = serde_json::from_slice(payload).unwrap();
    snapshot.as_object_mut().unwrap().remove("automations");
    let rewritten = rewrite_snapshot(&snapshot);
    std::fs::write(&path, &rewritten).unwrap();

    let restored = AppState::build(config).unwrap();
    let app = router(restored.clone());
    // The domain state is intact and the workspace simply has no automations.
    assert_eq!(
        get(&app, &format!("/api/v1/projects/{project}/automations"))
            .await
            .status(),
        StatusCode::OK
    );
    assert!(
        body_json(get(&app, "/api/v1/automations").await).await["automations"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    // The listed projects are the ones a user creates, and this workspace has
    // none. The personal scope is not among them — a client is handed it as
    // `personalProject` — but it still resolves by id, which the automations
    // request above proves.
    assert!(body_json(get(&app, "/api/v1/projects").await)
        .await
        .as_array()
        .unwrap()
        .is_empty());
    restored.shutdown();
}

/// Re-frames a snapshot payload the way `persistence.rs` does, so a test can
/// write a hand-edited snapshot the reader will accept.
fn rewrite_snapshot(snapshot: &Value) -> Vec<u8> {
    let payload = serde_json::to_vec(snapshot).unwrap();
    let mut out = Vec::with_capacity(payload.len() + 24);
    out.extend_from_slice(b"LOOMSNAP");
    out.extend_from_slice(&1u32.to_le_bytes());
    out.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    out.extend_from_slice(&crc32(&payload).to_le_bytes());
    out.extend_from_slice(&payload);
    out
}

/// CRC-32/ISO-HDLC, matching the snapshot framing.
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xedb8_8320 & mask);
        }
    }
    !crc
}

#[tokio::test]
async fn a_damaged_stored_row_is_reported_and_explained_not_dropped() {
    let (state, app, project, environment) = server().await;
    let automation = body_json(
        post(
            &app,
            &format!("/api/v1/projects/{project}/automations"),
            Some(create_body("damaged", &environment)),
        )
        .await,
    )
    .await["id"]
        .as_str()
        .unwrap()
        .to_owned();

    // A payload as a damaged snapshot might carry it: one row whose execution
    // no longer parses, and one whose legacy agent prompt is empty.
    let mut state_payload = AutomationState::current();
    state_payload.automations = vec![
        StoredAutomation {
            id: automation.clone(),
            project_id: project.clone(),
            name: "damaged".into(),
            enabled: true,
            trigger_type: Some("schedule".into()),
            trigger: json!({ "triggerType": "schedule", "cron": "0 9 * * *", "timezone": "UTC" }),
            run_mode: Some("agent".into()),
            execution: json!({ "mode": "telepathy" }),
            origin: Some("human".into()),
            created_at: 10,
            updated_at: 10,
            ..StoredAutomation::default()
        },
        StoredAutomation {
            id: "auto_00000000000000000000000000".into(),
            project_id: project.clone(),
            name: "legacy".into(),
            enabled: true,
            trigger_type: Some("schedule".into()),
            trigger: json!({ "triggerType": "schedule", "cron": "0 9 * * *", "timezone": "UTC" }),
            run_mode: Some("agent".into()),
            execution: json!({
                "mode": "agent",
                "prompt": "",
                "providerId": "pi",
                "model": "pi/default",
                "reasoningLevel": "medium",
                "permissionMode": "auto",
                "environment": { "type": "project-default" }
            }),
            origin: Some("human".into()),
            created_at: 20,
            updated_at: 20,
            ..StoredAutomation::default()
        },
    ];
    state.automations.restore(state_payload);

    let listed =
        body_json(get(&app, &format!("/api/v1/projects/{project}/automations")).await).await;
    assert_schema(
        &json!({ "type": "array", "items": contract::read_result() }),
        &listed,
        "list response",
    );
    assert_eq!(listed.as_array().unwrap().len(), 2);
    assert_eq!(listed[0]["problem"], "missing-agent-prompt");
    assert_eq!(listed[0]["name"], "legacy");
    assert_eq!(listed[0]["execution"]["prompt"], "");
    assert_eq!(listed[1]["problem"], "invalid-stored-data");
    assert_eq!(listed[1]["name"], "damaged");
    assert_eq!(listed[1]["id"], json!(automation));

    // A row that cannot be read is not silently written to: the operations
    // that would need it answer 409 instead.
    let response = post(
        &app,
        &format!("/api/v1/projects/{project}/automations/{automation}/pause"),
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let error = body_json(response).await;
    assert_eq!(error["code"], "conflict");
    assert!(error["message"]
        .as_str()
        .unwrap()
        .contains("invalid stored data"));
    let legacy_path = format!(
        "/api/v1/projects/{project}/automations/{}",
        listed[0]["id"].as_str().unwrap()
    );
    let response = post(&app, &format!("{legacy_path}/run"), None).await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert!(body_json(response).await["message"]
        .as_str()
        .unwrap()
        .contains("requires a prompt"));

    // …and an update that repairs the prompt is allowed through.
    let response = patch(
        &app,
        &legacy_path,
        json!({ "agent": { "prompt": "a real prompt" } }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let repaired = body_json(response).await;
    assert_schema(&contract::response(), &repaired, "repair response");
    assert_eq!(repaired["execution"]["prompt"], "a real prompt");
    state.shutdown();
}

#[tokio::test]
async fn a_run_page_is_cursored_newest_first() {
    let (state, app, project, environment) = server().await;
    let automation = body_json(
        post(
            &app,
            &format!("/api/v1/projects/{project}/automations"),
            Some(create_body("history", &environment)),
        )
        .await,
    )
    .await["id"]
        .as_str()
        .unwrap()
        .to_owned();

    // Three settled runs, as an execution plane's history would leave them.
    let mut payload = AutomationState::current();
    payload.automations = state.automations.export().automations;
    payload.runs = (0..3u64)
        .map(|index| StoredAutomationRun {
            id: format!("arun_{:0>26}", index),
            automation_id: automation.clone(),
            run_mode: "agent".into(),
            status: "succeeded".into(),
            trigger: "schedule".into(),
            output: Some(format!("output {index}")),
            exit_code: Some(0),
            scheduled_for: 1_700_000_000_000 + index,
            started_at: 1_700_000_000_000 + index,
            finished_at: Some(1_700_000_000_100 + index),
            ..StoredAutomationRun::default()
        })
        .collect();
    state.automations.restore(payload);

    let path = format!("/api/v1/projects/{project}/automations/{automation}/runs");
    let first = body_json(get(&app, &format!("{path}?limit=2")).await).await;
    assert_eq!(first["runs"].as_array().unwrap().len(), 2);
    assert_eq!(first["runs"][0]["startedAt"], 1_700_000_000_002u64);
    assert_eq!(first["runs"][1]["startedAt"], 1_700_000_000_001u64);
    let cursor = first["nextCursor"]
        .as_str()
        .expect("a second page exists")
        .to_owned();

    let second = body_json(get(&app, &format!("{path}?limit=2&cursor={cursor}")).await).await;
    assert_eq!(second["runs"].as_array().unwrap().len(), 1);
    assert_eq!(second["runs"][0]["startedAt"], 1_700_000_000_000u64);
    assert!(second["nextCursor"].is_null());
    state.shutdown();
}

/// A host environment with an `unmanaged` workspace must keep its `path` key.
///
/// The contract spells the workspace as a `.strict()` object whose `path` is
/// `z.string().min(1).nullable()`: the key is *required*, and only its value
/// may be null. A response that dropped the key when no path was set passed
/// every other assertion here — nothing else exercised a `host` environment —
/// yet failed the contract's own validation, so the omission was invisible to
/// both this suite and the response schema it validates against.
#[tokio::test]
async fn a_host_environment_response_keeps_the_contract_workspace_shape() {
    let (state, app, project, environment) = server().await;
    let host = loom_domain::HostId::mint().to_string();

    for (label, workspace, expect_path) in [
        (
            "no path",
            json!({ "type": "unmanaged", "path": null }),
            Value::Null,
        ),
        (
            "with a path",
            json!({ "type": "unmanaged", "path": "/srv/loom" }),
            json!("/srv/loom"),
        ),
    ] {
        let mut body = create_body(&format!("host {label}"), &environment);
        body["execution"]["environment"] = json!({
            "type": "host",
            "hostId": host,
            "workspace": workspace,
        });
        assert_schema(&contract::create_request(), &body, "create request");

        let response = post(
            &app,
            &format!("/api/v1/projects/{project}/automations"),
            Some(body),
        )
        .await;
        assert_eq!(response.status(), StatusCode::CREATED, "{label}");
        let created = body_json(response).await;
        // The whole response still satisfies the contract, which is what a
        // dropped `path` key breaks.
        assert_schema(&contract::response(), &created, "create response");

        let echoed = &created["execution"]["environment"]["workspace"];
        assert_eq!(echoed["type"], "unmanaged", "{label}");
        assert_eq!(echoed["path"], expect_path, "{label}");
        assert!(
            echoed.as_object().expect("an object").contains_key("path"),
            "{label}: the required `path` key is present: {echoed}"
        );

        // The stored round trip keeps it too: a read is the same projection.
        let fetched = body_json(
            get(
                &app,
                &format!(
                    "/api/v1/projects/{project}/automations/{}",
                    created["id"].as_str().unwrap()
                ),
            )
            .await,
        )
        .await;
        assert_schema(&contract::read_result(), &fetched, "get response");
        assert_eq!(
            fetched["execution"]["environment"]["workspace"]["path"], expect_path,
            "{label}"
        );
    }
    state.shutdown();
}

/* ------------------------------------------------------------------ */
/* Scheduling                                                          */
/* ------------------------------------------------------------------ */

/// Moves an automation's next window into the past, the state a server that
/// was not running when the window arrived restores into.
fn make_due(state: &AppState, automation: &str, window: u64) {
    let mut payload = state.automations.export();
    let row = payload
        .automations
        .iter_mut()
        .find(|row| row.id == automation)
        .expect("the automation is stored");
    row.next_run_at = Some(window);
    state.automations.restore(payload);
}

#[tokio::test]
async fn a_schedule_is_armed_in_its_zone_and_a_due_window_becomes_a_queued_run() {
    let (state, app, project, environment) = server().await;

    // Europe/Paris, every weekday at 09:00 local: what `nextRunAt` holds is
    // that wall clock, not the server's.
    let mut body = create_body("paris mornings", &environment);
    body["trigger"] = json!({
        "triggerType": "schedule",
        "cron": "0 9 * * 1-5",
        "timezone": "Europe/Paris"
    });
    let created = body_json(
        post(
            &app,
            &format!("/api/v1/projects/{project}/automations"),
            Some(body),
        )
        .await,
    )
    .await;
    assert_schema(&contract::response(), &created, "create response");
    let armed = created["nextRunAt"].as_u64().expect("an armed schedule");
    let paris = chrono_tz::Tz::Europe__Paris;
    let local = chrono::DateTime::from_timestamp_millis(armed as i64)
        .expect("a real instant")
        .with_timezone(&paris);
    assert_eq!(
        (
            chrono::Timelike::hour(&local),
            chrono::Timelike::minute(&local)
        ),
        (9, 0),
        "nextRunAt is 09:00 where the automation lives"
    );
    assert!(
        !matches!(
            chrono::Datelike::weekday(&local),
            chrono::Weekday::Sat | chrono::Weekday::Sun
        ),
        "`0 9 * * 1-5` is a weekday morning where the automation lives"
    );
    let automation = created["id"].as_str().unwrap().to_owned();

    // A window that arrived while the server was down is claimed once, and the
    // schedule moves past now rather than replaying the missed window.
    let window = armed - 86_400_000;
    make_due(&state, &automation, window);
    let report = state.sweep_automations(armed + 1_000);
    assert_eq!(report.claimed, 1);

    let runs = body_json(
        get(
            &app,
            &format!("/api/v1/projects/{project}/automations/{automation}/runs"),
        )
        .await,
    )
    .await;
    assert_schema(
        &json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["runs", "nextCursor"],
            "properties": {
                "runs": { "type": "array", "items": contract::run_response() },
                "nextCursor": { "type": ["string", "null"] }
            }
        }),
        &runs,
        "runs response",
    );
    assert_eq!(runs["runs"].as_array().unwrap().len(), 1);
    assert_eq!(runs["runs"][0]["trigger"], "schedule");
    assert_eq!(runs["runs"][0]["status"], "running");
    assert_eq!(runs["runs"][0]["scheduledFor"], json!(window));

    let fetched = body_json(
        get(
            &app,
            &format!("/api/v1/projects/{project}/automations/{automation}"),
        )
        .await,
    )
    .await;
    assert_eq!(fetched["runCount"], 1);
    assert_eq!(fetched["lastRunStatus"], "running");
    assert!(
        fetched["nextRunAt"].as_u64().unwrap() > armed + 1_000,
        "the next window is in the future"
    );

    // Sweeping again does not fire the same window twice.
    assert_eq!(state.sweep_automations(armed + 2_000).claimed, 0);
    let runs = body_json(
        get(
            &app,
            &format!("/api/v1/projects/{project}/automations/{automation}/runs"),
        )
        .await,
    )
    .await;
    assert_eq!(runs["runs"].as_array().unwrap().len(), 1);
    state.shutdown();
}

#[tokio::test]
async fn a_script_run_without_a_machine_fails_and_pausing_still_holds() {
    let (state, app, project, _environment) = server().await;
    // A *script* automation on a workspace where no machine is enrolled: the
    // run cannot be handed to anyone, so it fails with a reason a user can act
    // on rather than waiting forever.
    let mut body = create_body("queued", &loom_domain::EnvironmentId::mint());
    body["execution"] = json!({
        "mode": "script",
        "script": "echo hi",
        "interpreter": "bash",
        "timeoutMs": 120000
    });
    assert_schema(&contract::create_request(), &body, "create request");
    let automation = body_json(
        post(
            &app,
            &format!("/api/v1/projects/{project}/automations"),
            Some(body),
        )
        .await,
    )
    .await["id"]
        .as_str()
        .unwrap()
        .to_owned();

    let run = body_json(
        post(
            &app,
            &format!("/api/v1/projects/{project}/automations/{automation}/run"),
            Some(json!({ "idempotencyKey": "queued-1" })),
        )
        .await,
    )
    .await;
    assert_eq!(run["run"]["status"], "failed", "{run}");
    assert!(
        run["run"]["error"]
            .as_str()
            .is_some_and(|error| error.contains("machine") || error.contains("data directory")),
        "the failure should say why the script reached no machine: {run}"
    );
    assert_eq!(run["run"]["runMode"], "script");

    let paused = body_json(
        post(
            &app,
            &format!("/api/v1/projects/{project}/automations/{automation}/pause"),
            None,
        )
        .await,
    )
    .await;
    assert_eq!(paused["enabled"], false);
    assert!(paused["nextRunAt"].is_null());

    let runs = body_json(
        get(
            &app,
            &format!("/api/v1/projects/{project}/automations/{automation}/runs"),
        )
        .await,
    )
    .await;
    assert_schema(
        &json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["runs", "nextCursor"],
            "properties": {
                "runs": { "type": "array", "items": contract::run_response() },
                "nextCursor": { "type": ["string", "null"] }
            }
        }),
        &runs,
        "runs response",
    );
    assert_eq!(runs["runs"][0]["status"], "failed");
    assert!(runs["runs"][0]["finishedAt"].is_u64());

    // And nothing fires while it is paused, even with a window in the past.
    make_due(&state, &automation, 1_000);
    assert_eq!(state.sweep_automations(2_000).claimed, 0);
    state.shutdown();
}

#[tokio::test]
async fn a_claimed_window_is_not_replayed_and_its_interrupted_turn_fails_once() {
    let dir = TempDir::new().unwrap();
    let config = || AppConfig {
        backend_path: Some(dir.path().to_path_buf()),
        reconcile_interval: std::time::Duration::ZERO,
        snapshot_interval: std::time::Duration::ZERO,
        schedule_interval: std::time::Duration::ZERO,
        ..AppConfig::default()
    };
    let state = AppState::build(config()).unwrap();
    let app = router(state.clone());
    let project = state.registry.personal_project_id().to_string();
    let environment = executable_environment(&state);
    let automation = body_json(
        post(
            &app,
            &format!("/api/v1/projects/{project}/automations"),
            Some(create_body("durable", &environment)),
        )
        .await,
    )
    .await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    make_due(&state, &automation, 1_000);
    // The window becomes a run, and that run becomes a turn on the enrolled
    // host: what the restart finds in flight is a provider run.
    state.sweep_automations(2_000);
    let runs = body_json(
        get(
            &app,
            &format!("/api/v1/projects/{project}/automations/{automation}/runs"),
        )
        .await,
    )
    .await;
    assert_eq!(runs["runs"].as_array().unwrap().len(), 1);
    state.shutdown();

    let restored = AppState::build(config()).unwrap();
    let app = router(restored.clone());
    let runs = body_json(
        get(
            &app,
            &format!("/api/v1/projects/{project}/automations/{automation}/runs"),
        )
        .await,
    )
    .await;
    assert_eq!(
        runs["runs"].as_array().unwrap().len(),
        1,
        "one window, one run: the claimed window is not replayed"
    );
    assert_eq!(
        runs["runs"][0]["status"], "failed",
        "the turn an interrupted process cannot prove was running is failed once"
    );
    assert!(runs["runs"][0]["error"]
        .as_str()
        .is_some_and(|error| error.contains("restarted")));
    assert!(runs["runs"][0]["threadId"].is_string());

    // The window it was queued for is behind the schedule now, so a sweep
    // after the restart claims nothing.
    let report = restored.sweep_automations(3_000);
    assert_eq!(report.claimed, 0);
    assert_eq!(report.in_flight, 0, "the window is in the future, not due");
    let runs = body_json(
        get(
            &app,
            &format!("/api/v1/projects/{project}/automations/{automation}/runs"),
        )
        .await,
    )
    .await;
    assert_eq!(runs["runs"].as_array().unwrap().len(), 1);
    restored.shutdown();
}

#[tokio::test]
async fn a_payload_from_before_the_scheduler_reads_as_queued_work_after_a_restart() {
    let dir = TempDir::new().unwrap();
    let config = || AppConfig {
        backend_path: Some(dir.path().to_path_buf()),
        reconcile_interval: std::time::Duration::ZERO,
        snapshot_interval: std::time::Duration::ZERO,
        schedule_interval: std::time::Duration::ZERO,
        ..AppConfig::default()
    };
    let state = AppState::build(config()).unwrap();
    let app = router(state.clone());
    let project = state.registry.personal_project_id().to_string();
    let environment = executable_environment(&state);
    let automation = body_json(
        post(
            &app,
            &format!("/api/v1/projects/{project}/automations"),
            Some(create_body("legacy", &environment)),
        )
        .await,
    )
    .await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    state.shutdown();

    // Rewrite the snapshot the way the release before the scheduler wrote it:
    // version 1, no next window, and a run stored as `running` because nothing
    // could claim one yet.
    let path = dir.path().join("domain.snapshot");
    let bytes = std::fs::read(&path).unwrap();
    let header_len = 8 + 4 + 8 + 4;
    let mut snapshot: Value = serde_json::from_slice(&bytes[header_len..]).unwrap();
    let payload = snapshot["automations"].as_object_mut().unwrap();
    payload.insert("version".into(), json!(1));
    for row in payload["automations"].as_array_mut().unwrap() {
        row["nextRunAt"] = Value::Null;
    }
    payload["runs"] = json!([{
        "id": "arun_00000000000000000000000000",
        "automationId": automation,
        "runMode": "agent",
        "status": "running",
        "trigger": "manual",
        "scheduledFor": 1_000,
        "startedAt": 1_000
    }]);
    std::fs::write(&path, rewrite_snapshot(&snapshot)).unwrap();

    let restored = AppState::build(config()).unwrap();
    let app = router(restored.clone());
    let runs = body_json(
        get(
            &app,
            &format!("/api/v1/projects/{project}/automations/{automation}/runs"),
        )
        .await,
    )
    .await;
    assert_eq!(runs["runs"].as_array().unwrap().len(), 1);
    assert_eq!(
        runs["runs"][0]["status"], "running",
        "a pre-scheduler run was a queue entry, which is what it reads as now"
    );

    // The window it never had is armed by the sweep, so the automation resumes
    // its cadence instead of sitting inert.
    let report = restored.sweep_automations(2_000);
    assert_eq!(report.armed, 1);
    let fetched = body_json(
        get(
            &app,
            &format!("/api/v1/projects/{project}/automations/{automation}"),
        )
        .await,
    )
    .await;
    assert!(fetched["nextRunAt"]
        .as_u64()
        .is_some_and(|next| next > 2_000));
    restored.shutdown();
}
