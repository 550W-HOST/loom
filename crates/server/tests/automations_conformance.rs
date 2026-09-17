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
use loom_server::automations::{AutomationState, StoredAutomation, StoredAutomationRun};
use loom_server::http::router;
use loom_server::state::{AppConfig, AppState};
use serde_json::{json, Value};
use tempfile::TempDir;
use tower::ServiceExt;

/* ------------------------------------------------------------------ */
/* Contract shapes (loom-authored, from rpc-types.ts)                  */
/* ------------------------------------------------------------------ */

mod contract {
    use serde_json::{json, Value};

    fn string(min: u64) -> Value {
        json!({ "type": "string", "minLength": min })
    }

    pub fn branch_spec() -> Value {
        json!({
            "oneOf": [
                {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["kind", "name"],
                    "properties": { "kind": { "const": "existing" }, "name": string(1) }
                },
                {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["kind", "baseBranch"],
                    "properties": { "kind": { "const": "new" }, "baseBranch": string(1) }
                }
            ]
        })
    }

    pub fn workspace() -> Value {
        json!({
            "oneOf": [
                {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["type", "path"],
                    "properties": {
                        "type": { "const": "unmanaged" },
                        "path": { "type": ["string", "null"] },
                        "branch": branch_spec()
                    }
                },
                {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["type", "baseBranch"],
                    "properties": {
                        "type": { "const": "managed-worktree" },
                        "baseBranch": {
                            "oneOf": [
                                {
                                    "type": "object",
                                    "additionalProperties": false,
                                    "required": ["kind", "name"],
                                    "properties": { "kind": { "const": "named" }, "name": string(1) }
                                },
                                {
                                    "type": "object",
                                    "additionalProperties": false,
                                    "required": ["kind"],
                                    "properties": { "kind": { "const": "default" } }
                                }
                            ]
                        }
                    }
                },
                {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["type"],
                    "properties": { "type": { "const": "personal" } }
                }
            ]
        })
    }

    pub fn environment() -> Value {
        json!({
            "oneOf": [
                {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["type", "environmentId"],
                    "properties": { "type": { "const": "reuse" }, "environmentId": string(1) }
                },
                {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["type", "workspace"],
                    "properties": {
                        "type": { "const": "host" },
                        "hostId": string(1),
                        "workspace": workspace()
                    }
                },
                {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["type"],
                    "properties": { "type": { "const": "project-default" } }
                }
            ]
        })
    }

    pub fn trigger() -> Value {
        json!({
            "oneOf": [
                {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["triggerType", "cron", "timezone"],
                    "properties": {
                        "triggerType": { "const": "schedule" },
                        "cron": { "type": "string", "minLength": 1, "maxLength": 100 },
                        "timezone": { "type": "string", "minLength": 1, "maxLength": 100 }
                    }
                },
                {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["triggerType", "runAt"],
                    "properties": {
                        "triggerType": { "const": "once" },
                        "runAt": { "type": "integer", "minimum": 1 }
                    }
                }
            ]
        })
    }

    pub fn execution() -> Value {
        execution_with_prompt(1)
    }

    /// The agent execution schema with a chosen prompt bound.
    ///
    /// The legacy `missing-agent-prompt` variant is the same shape with an
    /// empty prompt allowed: a row that reads is still readable.
    fn execution_with_prompt(prompt_min_length: u64) -> Value {
        json!({
            "oneOf": [
                {
                    "type": "object",
                    "additionalProperties": false,
                    "required": [
                        "mode", "prompt", "providerId", "model", "reasoningLevel",
                        "permissionMode", "environment"
                    ],
                    "properties": {
                        "mode": { "const": "agent" },
                        "prompt": { "type": "string", "minLength": prompt_min_length },
                        "providerId": string(1),
                        "model": string(1),
                        "reasoningLevel": {
                            "enum": ["none", "low", "medium", "high", "xhigh", "ultracode", "max", "ultra"]
                        },
                        "serviceTier": { "enum": ["default", "fast"] },
                        "permissionMode": { "enum": ["accept-edits", "auto", "full"] },
                        "environment": environment(),
                        "targetThreadId": string(1)
                    }
                },
                {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["mode", "timeoutMs"],
                    "properties": {
                        "mode": { "const": "script" },
                        "script": { "type": "string", "minLength": 1, "maxLength": 262144 },
                        "scriptFile": { "type": "string", "minLength": 1, "maxLength": 200 },
                        "interpreter": { "enum": ["bash", "sh", "node", "python3"] },
                        "timeoutMs": { "type": "integer", "minimum": 1, "maximum": 900000 },
                        "env": { "type": "object" }
                    }
                }
            ]
        })
    }

    fn nullable_string() -> Value {
        json!({ "type": ["string", "null"] })
    }

    fn nullable_number() -> Value {
        json!({ "type": ["integer", "null"] })
    }

    pub fn response() -> Value {
        response_with_prompt(1)
    }

    /// The response schema with a chosen prompt bound, so the legacy empty
    /// prompt variant can be described without duplicating the shape.
    fn response_with_prompt(prompt_min_length: u64) -> Value {
        json!({
            "type": "object",
            "additionalProperties": false,
            "required": [
                "id", "projectId", "name", "enabled", "trigger", "execution", "origin",
                "createdByThreadId", "nextRunAt", "lastRunAt", "runCount", "lastRunStatus",
                "lastRunThreadId", "lastError", "createdAt", "updatedAt"
            ],
            "properties": {
                "id": string(1),
                "projectId": string(1),
                "name": string(1),
                "enabled": { "type": "boolean" },
                "trigger": trigger(),
                "execution": execution_with_prompt(prompt_min_length),
                "origin": { "enum": ["human", "app", "agent"] },
                "createdByThreadId": nullable_string(),
                "nextRunAt": nullable_number(),
                "lastRunAt": nullable_number(),
                "runCount": { "type": "integer", "minimum": 0 },
                "lastRunStatus": { "type": ["string", "null"], "enum": ["running", "succeeded", "failed", "skipped", null] },
                "lastRunThreadId": nullable_string(),
                "lastError": nullable_string(),
                "createdAt": { "type": "integer" },
                "updatedAt": { "type": "integer" }
            }
        })
    }

    pub fn read_result() -> Value {
        json!({
            "anyOf": [
                response(),
                {
                    "allOf": [
                        response_with_prompt(0),
                        {
                            "type": "object",
                            "required": ["problem"],
                            "properties": { "problem": { "const": "missing-agent-prompt" } }
                        }
                    ]
                },
                {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["id", "projectId", "name", "problem"],
                    "properties": {
                        "id": { "type": "string" },
                        "projectId": { "type": "string" },
                        "name": { "type": "string" },
                        "problem": { "const": "invalid-stored-data" }
                    }
                }
            ]
        })
    }

    pub fn run_response() -> Value {
        json!({
            "type": "object",
            "additionalProperties": false,
            "required": [
                "id", "automationId", "runMode", "threadId", "status", "trigger",
                "skipReason", "error", "output", "exitCode", "scheduledFor", "startedAt",
                "finishedAt"
            ],
            "properties": {
                "id": string(1),
                "automationId": string(1),
                "runMode": { "enum": ["agent", "script"] },
                "threadId": nullable_string(),
                "status": { "enum": ["running", "succeeded", "failed", "skipped"] },
                "trigger": { "enum": ["schedule", "manual"] },
                "skipReason": nullable_string(),
                "error": nullable_string(),
                "output": nullable_string(),
                "exitCode": nullable_number(),
                "scheduledFor": { "type": "integer" },
                "startedAt": { "type": "integer" },
                "finishedAt": nullable_number()
            }
        })
    }

    pub fn create_request() -> Value {
        json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["name", "trigger", "execution", "origin"],
            "properties": {
                "name": { "type": "string", "minLength": 1, "maxLength": 200 },
                "enabled": { "type": "boolean" },
                "trigger": trigger(),
                "execution": execution(),
                "origin": { "enum": ["human", "app", "agent"] },
                "createdByThreadId": string(1)
            }
        })
    }

    pub fn update_request() -> Value {
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "name": { "type": "string", "minLength": 1, "maxLength": 200 },
                "trigger": trigger(),
                "execution": execution(),
                "agent": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "prompt": string(1),
                        "providerId": string(1),
                        "model": string(1),
                        "reasoningLevel": {
                            "enum": ["none", "low", "medium", "high", "xhigh", "ultracode", "max", "ultra"]
                        },
                        "serviceTier": { "type": ["string", "null"], "enum": ["default", "fast", null] },
                        "permissionMode": { "enum": ["accept-edits", "auto", "full"] },
                        "target": {
                            "oneOf": [
                                {
                                    "type": "object",
                                    "additionalProperties": false,
                                    "required": ["type", "threadId"],
                                    "properties": { "type": { "const": "target-thread" }, "threadId": string(1) }
                                },
                                {
                                    "type": "object",
                                    "additionalProperties": false,
                                    "required": ["type", "environment"],
                                    "properties": { "type": { "const": "environment" }, "environment": environment() }
                                }
                            ]
                        }
                    }
                }
            }
        })
    }

    pub fn run_request() -> Value {
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": { "idempotencyKey": string(1) }
        })
    }
}

/* ------------------------------------------------------------------ */
/* Helpers                                                             */
/* ------------------------------------------------------------------ */

/// Validates an instance against a loom-authored automation schema.
///
/// The schemas declare no `$ref`, so the document root is irrelevant; the
/// validator itself is the contract one.
#[track_caller]
fn assert_schema(schema: &Value, instance: &Value, what: &str) {
    let violations = loom_contract::validate(&json!({}), schema, instance);
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
fn agent_execution() -> Value {
    json!({
        "mode": "agent",
        "prompt": "summarise the repository",
        "providerId": "pi",
        "model": "pi/default",
        "reasoningLevel": "medium",
        "permissionMode": "auto",
        "environment": { "type": "project-default" }
    })
}

/// A create body, in the contract's shape.
fn create_body(name: &str) -> Value {
    let body = json!({
        "name": name,
        "trigger": { "triggerType": "schedule", "cron": "0 9 * * 1-5", "timezone": "Europe/Paris" },
        "execution": agent_execution(),
        "origin": "human"
    });
    assert_schema(&contract::create_request(), &body, "create request");
    body
}

/// The ephemeral server the route tests run against.
async fn server() -> (AppState, Router, String) {
    let state = AppState::build(AppConfig::default()).unwrap();
    let app = router(state.clone());
    let project = state.registry.personal_project_id().to_string();
    (state, app, project)
}

/* ------------------------------------------------------------------ */
/* Tests                                                              */
/* ------------------------------------------------------------------ */

#[tokio::test]
async fn automations_crud_overview_and_history_are_contract_shaped() {
    let (state, app, project) = server().await;

    // create
    let response = post(
        &app,
        &format!("/api/v1/projects/{project}/automations"),
        Some(create_body("nightly")),
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
    // A cron schedule has no computed instant yet: the scheduler stage owns it.
    assert!(created["nextRunAt"].is_null());
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
    assert!(run["run"]["threadId"].is_null());
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
    let (state, app, project) = server().await;
    let future = 4_000_000_000_000u64;

    let mut body = create_body("one shot");
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

    let mut past = create_body("too late");
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
    let mut both_script_sources = create_body("scripted");
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

    let mut bad_cron = create_body("bad cron");
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

    let mut bad_timezone = create_body("bad timezone");
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
    let (state, app, project) = server().await;

    // An unknown field is not silently dropped. Rejecting a body that does not
    // match the request shape is the framework's job, so it is a 422 in the
    // same uniform error shape the contract middleware uses.
    let mut unknown_field = create_body("strict");
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
    let mut unknown_inner = create_body("strict inner");
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
            Some(create_body("targets")),
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
        json!({ "execution": agent_execution(), "agent": { "model": "pi/other" } }),
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
    let (state, app, project) = server().await;
    let automation = body_json(
        post(
            &app,
            &format!("/api/v1/projects/{project}/automations"),
            Some(create_body("scoped")),
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

    let created = body_json(
        post(
            &app,
            &format!("/api/v1/projects/{project}/automations"),
            Some(create_body("durable")),
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
    assert_eq!(fetched, created);

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
    let app = router(state.clone());
    post(
        &app,
        &format!("/api/v1/projects/{project}/automations"),
        Some(create_body("kept")),
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
    assert_eq!(
        body_json(get(&app, "/api/v1/projects").await)
            .await
            .as_array()
            .unwrap()
            .len(),
        1
    );
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
    let (state, app, project) = server().await;
    let automation = body_json(
        post(
            &app,
            &format!("/api/v1/projects/{project}/automations"),
            Some(create_body("damaged")),
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
    let (state, app, project) = server().await;
    let automation = body_json(
        post(
            &app,
            &format!("/api/v1/projects/{project}/automations"),
            Some(create_body("history")),
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
