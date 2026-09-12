//! HTTP surface.
//!
//! Small on purpose. The interesting surface is the WebSocket; these routes
//! exist so a server can be probed, identified and fed events.

use axum::body::Body;
use axum::extract::{Path, Query, Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use http_body_util::BodyExt;
use loom_domain::{
    DomainError, DomainEvent, DomainScope, Environment, EnvironmentId, EnvironmentKind,
    EnvironmentStatus, Host, HostId, HostStatus, MessageRole, Project, ProjectId, ProjectKind,
    ProjectSourceId, Thread, ThreadId, ThreadStatus,
};
use loom_relay::scope::Scope;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::domain_state::CommandError;
use crate::state::{domain_event_from_envelope, AppState};
use crate::ui;
use crate::ws;
use crate::{artifacts, PROTOCOL_VERSION};

/// Builds the router for a wired-up state.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/api/v1/version", get(version))
        .route("/api/v1/sidebar-bootstrap", get(sidebar_bootstrap))
        .route("/api/v1/system/config", get(system_config))
        .route(
            "/api/v1/system/environment-providers",
            get(environment_providers),
        )
        .route("/api/v1/system/execution-options", get(execution_options))
        .route("/api/v1/system/providers", get(system_providers))
        .route(
            "/api/v1/system/providers/state",
            get(system_provider_states),
        )
        .route("/api/v1/system/version", get(system_version))
        .route("/api/v1/publish", post(publish))
        .route("/api/v1/replay", get(replay))
        .route("/api/v1/threads", get(list_threads).post(create_thread))
        .route("/api/v1/threads/{id}/events", get(thread_events))
        .route("/api/v1/threads/{id}", get(get_thread))
        .route("/api/v1/threads/{id}/output", get(thread_output))
        .route("/api/v1/threads/{id}/read", post(read_thread))
        .route("/api/v1/threads/{id}/send", post(send_thread))
        .route("/api/v1/threads/{id}/tabs", get(thread_tabs))
        .route("/api/v1/threads/{id}/timeline", get(thread_timeline))
        .route("/api/v1/threads/{id}/messages", post(post_thread_message))
        .route("/api/v1/projects", get(list_projects).post(create_project))
        .route(
            "/api/v1/projects/{id}",
            get(get_project).patch(update_project),
        )
        .route("/api/v1/projects/{id}/archive", post(archive_project))
        .route("/api/v1/projects/{id}/sources", post(add_project_source))
        .route(
            "/api/v1/projects/{id}/sources/{source_id}",
            axum::routing::delete(remove_project_source),
        )
        .route(
            "/api/v1/environments",
            get(list_environments).post(create_environment),
        )
        .route("/api/v1/environments/{id}", get(get_environment))
        .route(
            "/api/v1/environments/{id}/provision",
            post(provision_environment),
        )
        .route(
            "/api/v1/environments/{id}/destroy",
            post(destroy_environment),
        )
        .route("/api/v1/runs", get(list_runs))
        .route("/api/v1/hosts", get(list_hosts).post(register_host))
        .route("/api/v1/hosts/primary", get(primary_host))
        .route("/api/v1/hosts/{id}/heartbeat", post(host_heartbeat))
        .route("/api/v1/hosts/{id}/disconnect", post(disconnect_host))
        .route("/ws", get(ws::client_socket))
        // Daemon self-update: the version to compare against, and the binary
        // that matches it. Deliberately not behind the `/api` namespace — a
        // daemon fetching its own replacement is not a domain operation — and
        // declared here as literals so `scripts/check-api-coverage.mjs` can
        // parse them and record them as the contract-external entries they are
        // (`artifacts::INSTALL_VERSION_PATH` and its sibling are what the
        // handlers and the daemon client use).
        .route("/install/version", get(artifacts::install_version))
        .route("/install/loom-daemon", get(artifacts::install_daemon))
        // Everything else is a client route: the UI shell (or a dev-server
        // proxy). API and socket paths are excluded inside the handler.
        .fallback(ui::serve)
        .layer(middleware::from_fn(validate_contract_request))
        .layer(middleware::from_fn(normalize_api_error))
        .with_state(state)
}

/// Rejects a request whose JSON body does not match the bb contract.
///
/// The response half of conformance is asserted by tests; the request half
/// cannot be, because a handler that silently reshapes its body still returns
/// the right JSON. This middleware closes that gap at runtime: for every
/// implemented bb route with a JSON request schema, the parsed body is
/// validated before the handler sees it, and a mismatch is a `422` in the same
/// uniform error shape.
///
/// It is deliberately a no-op for everything else — contract-external loom
/// routes, non-JSON bodies, malformed JSON (the extractor reports that) — so
/// it can only reject a request that reached a contract route.
async fn validate_contract_request(request: Request, next: Next) -> Response {
    let Some(route) =
        loom_contract::shared().match_route(request.method().as_str(), request.uri().path())
    else {
        return next.run(request).await;
    };
    if route.request.source != "json" || route.request.schema.is_none() {
        return next.run(request).await;
    }
    let (parts, body) = request.into_parts();
    let bytes = match body.collect().await {
        Ok(body) => body.to_bytes(),
        Err(error) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                format!("failed to read the request body: {error}"),
            )
        }
    };
    // A body the JSON extractor will reject anyway gets its own message from
    // `normalize_api_error`; re-running the handler preserves that behaviour.
    let instance: Value = match serde_json::from_slice(&bytes) {
        Ok(instance) => instance,
        Err(_) => {
            return next
                .run(Request::from_parts(parts, Body::from(bytes)))
                .await;
        }
    };
    let violations = loom_contract::shared().validate_request(route, &instance);
    if !violations.is_empty() {
        return error_response_with_code(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_request",
            format!(
                "request body does not match the `{}` contract: {}",
                route.id,
                loom_contract::describe(&violations)
            ),
        );
    }
    next.run(Request::from_parts(parts, Body::from(bytes)))
        .await
}

/// Keeps extractor failures on API routes in the same shape as handler errors.
/// Axum's JSON and query extractors otherwise return a plain-text rejection.
async fn normalize_api_error(request: Request, next: Next) -> Response {
    let is_api = request.uri().path().starts_with("/api/");
    let response = next.run(request).await;
    if !is_api || (!response.status().is_client_error() && !response.status().is_server_error()) {
        return response;
    }

    let status = response.status();
    let (parts, body) = response.into_parts();
    let bytes = match body.collect().await {
        Ok(body) => body.to_bytes(),
        Err(error) => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to read an API error response: {error}"),
            )
        }
    };
    let already_shaped = serde_json::from_slice::<Value>(&bytes)
        .ok()
        .and_then(|value| value.as_object().cloned())
        .is_some_and(|object| {
            object.get("code").is_some_and(Value::is_string)
                && object.get("message").is_some_and(Value::is_string)
        });
    if already_shaped {
        return Response::from_parts(parts, Body::from(bytes));
    }

    let message = String::from_utf8_lossy(&bytes).trim().to_owned();
    let message = if message.is_empty() {
        format!("request failed with status {status}")
    } else {
        message
    };
    error_response(status, message)
}

/// Liveness and basic introspection.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct HealthResponse {
    /// Always `"ok"` when the process can answer at all.
    pub status: &'static str,
    /// Relay protocol version.
    pub protocol_version: u32,
    /// Node identity.
    pub node_id: String,
    /// Milliseconds since startup.
    pub uptime_ms: u64,
    /// Fixed number of shard readers.
    pub readers: usize,
    /// Records currently retained across all shards.
    pub retained_events: usize,
    /// The backend's latched failure, if it has one.
    ///
    /// Present means the process is serving from memory and its durability is
    /// degraded: the frame the log just accepted may not survive a restart.
    /// Reads continue to work by design, so this field — not a missing
    /// response — is how an operator learns about it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backend_error: Option<String>,
}

async fn health(State(state): State<AppState>) -> Response {
    let retained_events = match state.relay.retained() {
        Ok(count) => count,
        Err(error) => {
            return error_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string());
        }
    };
    Json(HealthResponse {
        // Liveness is about answering at all; a degraded backend is reported
        // in `backend_error` rather than by taking the process out of service.
        status: "ok",
        protocol_version: PROTOCOL_VERSION,
        node_id: state.relay.origin().to_string(),
        uptime_ms: state.uptime_ms(),
        readers: state.pump.reader_count(),
        retained_events,
        backend_error: state.relay.backend_error(),
    })
    .into_response()
}

/// Build identity.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct VersionResponse {
    /// Crate version.
    pub version: &'static str,
    /// Relay protocol version.
    pub protocol_version: u32,
}

async fn version() -> Json<VersionResponse> {
    Json(VersionResponse {
        version: env!("CARGO_PKG_VERSION"),
        protocol_version: PROTOCOL_VERSION,
    })
}

/// Query fields shared by the provider discovery routes. The current provider
/// registry is process configuration, so these filters are accepted for wire
/// compatibility and do not change the single configured provider result.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
struct ProviderQuery {
    capability: Option<String>,
    environment_id: Option<String>,
    host_id: Option<String>,
    project_id: Option<String>,
    provider_id: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[allow(dead_code)]
struct SystemVersionQuery {
    force: Option<String>,
}

fn configured_provider_id(state: &AppState) -> String {
    state.provider_spec().name.clone()
}

fn configured_provider_info(state: &AppState) -> Value {
    let provider_id = configured_provider_id(state);
    json!({
        "id": provider_id,
        // `pluginId` remains a required bb field even though loom providers
        // are first-class and do not use a plugin lifecycle.
        "pluginId": "loom",
        "displayName": "Pi",
        "logoUrl": null,
        "maintenance": {
            "health": true,
            "usage": true,
            "installation": false
        },
        "capabilities": {
            "supportsThreadArchive": true,
            "supportsThreadRename": false,
            "supportsServiceTier": false,
            "supportsNativeUserQuestion": false,
            "supportsFork": false,
            "supportsSessionRewind": false,
            "permissionModes": ["accept-edits", "auto", "full"],
            "modelCatalogScope": "workspace"
        },
        "composerActions": [],
        "available": true
    })
}

fn configured_model(state: &AppState) -> Value {
    let provider_id = configured_provider_id(state);
    json!({
        "id": format!("{provider_id}/default"),
        "model": provider_id,
        "displayName": "Default",
        "description": "The configured loom provider model",
        "supportedReasoningEfforts": [{
            "reasoningEffort": "medium",
            "description": "Balanced reasoning"
        }],
        "defaultReasoningEffort": "medium",
        "isDefault": true
    })
}

fn configured_execution_options(state: &AppState) -> Value {
    let model = configured_model(state);
    json!({
        "providers": [configured_provider_info(state)],
        "permissionCeiling": "full",
        "models": [model.clone()],
        "selectedOnlyModels": [model],
        "modelLoadError": null
    })
}

/// Returns the project/thread data needed to hydrate the sidebar in one call.
async fn sidebar_bootstrap(State(state): State<AppState>) -> Json<Value> {
    let projects = state.registry.projects();
    let personal_id = state.registry.personal_project_id();
    let personal_project = projects
        .iter()
        .find(|project| project.id == personal_id)
        .map(|project| project_detail_value(&state, project))
        .unwrap_or_else(|| {
            json!({
                "id": personal_id.to_string(),
                "kind": "personal",
                "name": "Personal",
                "gitRemoteUrl": null,
                "createdAt": 0,
                "updatedAt": 0,
                "sources": [],
                "threads": [],
                "defaultExecutionOptions": null
            })
        });
    let projects = projects
        .iter()
        .map(|project| project_detail_value(&state, project))
        .collect::<Vec<_>>();
    Json(json!({
        "sections": [],
        "projects": projects,
        "personalProject": personal_project
    }))
}

/// Returns the static configuration surface required by the bb client.
async fn system_config(State(state): State<AppState>) -> Json<Value> {
    let provider_id = configured_provider_id(&state);
    Json(json!({
        "generalSettings": {
            "showKeyboardHints": true,
            "steerActiveThreadOnEnter": true,
            "showDiagnosticEvents": false,
            "providerOrder": [provider_id],
            "defaultProviderId": configured_provider_id(&state),
            "streamerMode": false,
            "managedBranchPrefix": ""
        },
        "keybindings": [],
        "defaultKeybindings": [],
        "keybindingOverrides": [],
        "experiments": {
            "changelogPreview": false,
            "mobileApp": false,
            "sidebarProgressiveDisclosure": false,
            "timelineWindowing": true
        },
        "appearance": {
            "themeId": "default",
            "customCss": null,
            "faviconColor": "default",
            "resolvedCodeTheme": {
                "dark": "",
                "light": "",
                "files": {}
            }
        },
        "customThemes": [],
        "pluginThemes": [],
        "featureFlags": {
            "placeholder": false,
            "timelineWindowEventBudget": 500
        },
        "hostDaemonPort": null,
        "localHelperPorts": [],
        "serverUrl": "",
        "primaryHostId": state.local_host_id().map(ToString::to_string),
        "primaryHostPlatform": null,
        "voiceTranscriptionEnabled": false,
        "aiServices": {
            "inference": "",
            "inferenceFallback": "",
            "transcription": "",
            "services": []
        },
        "dataDir": ""
    }))
}

/// Environment providers are intentionally empty until the environment
/// provider domain is introduced. An empty catalog is a valid bb response and
/// avoids claiming that the execution provider can provision workspaces.
async fn environment_providers(
    State(_state): State<AppState>,
    Query(_query): Query<ProviderQuery>,
) -> Json<Value> {
    Json(json!({ "providers": [] }))
}

async fn execution_options(
    State(state): State<AppState>,
    Query(_query): Query<ProviderQuery>,
) -> Json<Value> {
    Json(configured_execution_options(&state))
}

async fn system_providers(
    State(state): State<AppState>,
    Query(_query): Query<ProviderQuery>,
) -> Json<Value> {
    Json(json!([configured_provider_info(&state)]))
}

async fn system_provider_states(
    State(state): State<AppState>,
    Query(_query): Query<ProviderQuery>,
) -> Json<Value> {
    let provider_id = configured_provider_id(&state);
    Json(json!({
        "providers": [{
            "status": "unknown",
            "statusMessage": null,
            "accountEmail": null,
            "planLabel": null,
            "installedVersion": null,
            "minimumSupportedVersion": null,
            "canInstall": false,
            "canUpdate": false,
            "loginCommand": null,
            "providerId": provider_id,
            "displayName": "Pi"
        }]
    }))
}

async fn system_version(Query(_query): Query<SystemVersionQuery>) -> Json<Value> {
    Json(json!({
        "currentVersion": env!("CARGO_PKG_VERSION"),
        "latestVersion": null,
        "source": "npm",
        "updateAvailable": false,
        "isDevelopment": true,
        "upgradeCommand": ""
    }))
}

fn project_source_value(source: &loom_domain::ProjectSource) -> Value {
    json!({
        "id": source.id.to_string(),
        "projectId": source.project_id.to_string(),
        "isDefault": source.is_default,
        "createdAt": source.created_at_ms,
        "updatedAt": source.updated_at_ms,
        "type": "local_path",
        "hostId": source.host_id.to_string(),
        "path": source.path
    })
}

/// A project in bb's `projectSchema` shape (`$defs/d627`).
///
/// The contract names the fields in camelCase and omits loom's
/// `archived_at_ms`; archiving is carried by `projects.list`'s own filter, so
/// this projection is the whole representation.
fn project_value(project: &Project) -> Value {
    json!({
        "id": project.id.to_string(),
        "kind": project.kind,
        "name": project.name,
        "gitRemoteUrl": project.git_remote_url,
        "createdAt": project.created_at_ms,
        "updatedAt": project.updated_at_ms,
        "sources": project.sources.iter().map(project_source_value).collect::<Vec<_>>(),
    })
}

/// A project with its threads, for the sidebar bootstrap (`$defs/d526`).
fn project_detail_value(state: &AppState, project: &Project) -> Value {
    let threads = state
        .registry
        .threads()
        .into_iter()
        .filter(|thread| thread.project_id == project.id)
        .map(|thread| thread_list_entry_value(state, &thread))
        .collect::<Vec<_>>();
    let mut value = project_value(project);
    let object = value.as_object_mut().expect("project is an object");
    object.insert("threads".into(), Value::Array(threads));
    object.insert("defaultExecutionOptions".into(), Value::Null);
    value
}

/// An environment in bb's `environmentSchema` shape (`$defs/d52`).
///
/// Fields bb computes from git state that loom does not track yet are `null`
/// rather than omitted: the contract requires them, and a client reads `null`
/// as "unknown", which is the truth.
fn environment_value(environment: &Environment) -> Value {
    json!({
        "id": environment.id.to_string(),
        "name": environment.name,
        "projectId": environment.project_id.to_string(),
        "hostId": environment.host_id.to_string(),
        "path": environment.path,
        "isGitRepo": false,
        "isWorktree": environment.kind == EnvironmentKind::Managed,
        "branchName": null,
        "baseBranch": null,
        "defaultBranch": null,
        "mergeBaseBranch": null,
        "status": environment.status,
        "environmentProviderId": null,
        "lifecycle": { "phase": "active", "retireAt": null, "teardown": null },
        "environmentProviderSelection": null,
        "environmentProviderInstanceKey": null,
        "managed": environment.kind == EnvironmentKind::Managed,
        "workspaceProvisionType": match environment.kind {
            EnvironmentKind::Managed => "managed-worktree",
            EnvironmentKind::Unmanaged => "unmanaged",
        },
        "createdAt": environment.created_at_ms,
        "updatedAt": environment.updated_at_ms,
    })
}

/// A host in bb's `hostSchema` shape (`$defs/d582`).
fn host_value(host: &Host) -> Value {
    json!({
        "id": host.id.to_string(),
        "name": host.name,
        "type": "persistent",
        "status": host.status,
        "maxPermissionMode": "full",
        "lastSeenAt": host.last_seen_at_ms,
        "lastRejectedProtocolVersion": null,
        "createdAt": host.created_at_ms,
        "updatedAt": host.updated_at_ms,
    })
}

fn bb_thread_status(status: ThreadStatus) -> &'static str {
    match status {
        ThreadStatus::Idle | ThreadStatus::Archived => "idle",
        ThreadStatus::Working | ThreadStatus::Waiting => "active",
        ThreadStatus::Error => "error",
    }
}

fn runtime_display_status(
    state: &AppState,
    thread: &Thread,
    environment: Option<&Environment>,
) -> &'static str {
    match thread.status {
        ThreadStatus::Idle | ThreadStatus::Archived => "idle",
        ThreadStatus::Error => "error",
        ThreadStatus::Waiting => "active",
        ThreadStatus::Working => match environment {
            Some(environment) => match state.registry.host(&environment.host_id) {
                Some(host) if host.status == HostStatus::Connected => "active",
                Some(_) => "host-reconnecting",
                None => "waiting-for-host",
            },
            None => "waiting-for-host",
        },
    }
}

fn thread_summary_value(state: &AppState, thread: &Thread) -> Value {
    let environment = thread
        .environment_id
        .as_ref()
        .and_then(|id| state.registry.environment(id));
    let environment_id = thread.environment_id.as_ref().map(ToString::to_string);
    json!({
        "id": thread.id.to_string(),
        "projectId": thread.project_id.to_string(),
        "environmentId": environment_id,
        "providerId": configured_provider_id(state),
        "title": thread.title,
        "titleFallback": thread.title,
        "sectionId": null,
        "status": bb_thread_status(thread.status),
        "parentThreadId": thread.parent_thread_id.as_ref().map(ToString::to_string),
        "sourceThreadId": null,
        "originKind": null,
        "originPluginId": null,
        "visibility": if thread.status == ThreadStatus::Archived { "hidden" } else { "visible" },
        "archivedAt": thread.archived_at_ms,
        "pinnedAt": null,
        "deletedAt": null,
        "lastReadAt": thread.last_read_at_ms,
        "latestAttentionAt": thread.updated_at_ms,
        "createdAt": thread.created_at_ms,
        "updatedAt": thread.updated_at_ms,
        "runtime": {
            "displayStatus": runtime_display_status(state, thread, environment.as_ref()),
            "hostReconnectGraceExpiresAt": null
        },
        "activeBackgroundAgentCount": 0,
        "canSpawnChild": true,
        "queuedMessageCount": 0
    })
}

fn thread_list_entry_value(state: &AppState, thread: &Thread) -> Value {
    let summary = thread_summary_value(state, thread);
    let environment = thread
        .environment_id
        .as_ref()
        .and_then(|id| state.registry.environment(id));
    let (
        environment_host_id,
        environment_name,
        environment_path,
        environment_is_worktree,
        environment_workspace_display_kind,
    ) = match environment.as_ref() {
        Some(environment) => (
            Some(environment.host_id.to_string()),
            environment.name.clone(),
            environment.path.clone(),
            Some(environment.kind == EnvironmentKind::Managed),
            if environment.kind == EnvironmentKind::Managed {
                "managed-worktree"
            } else {
                "unmanaged-worktree"
            },
        ),
        None => (None, None, None, None, "other"),
    };
    let mut value = summary;
    let object = value.as_object_mut().expect("thread summary is an object");
    object.insert(
        "activity".into(),
        json!({
            "activeWorkflowCount": 0,
            "activeBackgroundAgentCount": 0,
            "activeBackgroundCommandCount": 0,
            "activePlanModeCount": 0,
            "activeGoalCount": 0
        }),
    );
    object.insert(
        "queuedWork".into(),
        json!(if thread.status == ThreadStatus::Error {
            "failed"
        } else {
            "none"
        }),
    );
    object.insert("pinSortKey".into(), Value::Null);
    object.insert("hasPendingInteraction".into(), Value::Bool(false));
    object.insert(
        "environmentHostId".into(),
        environment_host_id.map_or(Value::Null, Value::String),
    );
    object.insert(
        "environmentName".into(),
        environment_name.map_or(Value::Null, Value::String),
    );
    object.insert("environmentBranchName".into(), Value::Null);
    object.insert(
        "environmentPath".into(),
        environment_path.map_or(Value::Null, Value::String),
    );
    object.insert("environmentProviderId".into(), Value::Null);
    object.insert(
        "environmentIsWorktree".into(),
        environment_is_worktree.map_or(Value::Null, Value::Bool),
    );
    object.insert(
        "environmentWorkspaceDisplayKind".into(),
        Value::String(environment_workspace_display_kind.into()),
    );
    // `threadListEntrySchema` is a narrower superset of `threadSchema`: the
    // list row has the environment/activity columns instead of the three
    // single-thread counters, and `additionalProperties: false` means
    // leaving them in rejects the whole row.
    for key in [
        "activeBackgroundAgentCount",
        "canSpawnChild",
        "queuedMessageCount",
    ] {
        object.remove(key);
    }
    value
}

/// Body of a publish request.
#[derive(Clone, Debug, Deserialize)]
pub struct PublishRequest {
    /// The scope to publish to.
    pub scope: Scope,
    /// The frame, as text.
    pub payload: String,
}

/// Acknowledges a publish with the event's identity.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct PublishResponse {
    /// The assigned event id, usable as a resume cursor.
    pub event_id: String,
    /// The scope the event was stored under.
    pub scope: Scope,
    /// Producer wall-clock time.
    pub created_at_ms: u64,
}

/// Stores a frame and wakes the readers.
///
/// This is the generic producer entry point. Feature routes (creating a thread,
/// sending a message) will publish through the same path rather than inventing
/// their own delivery.
async fn publish(State(state): State<AppState>, Json(request): Json<PublishRequest>) -> Response {
    match state.publish(request.scope, request.payload) {
        Ok(envelope) => Json(PublishResponse {
            event_id: envelope.event_id.to_string(),
            scope: envelope.scope,
            created_at_ms: envelope.created_at_ms,
        })
        .into_response(),
        Err(error) => error_response(StatusCode::BAD_REQUEST, error.to_string()),
    }
}

fn error_response(status: StatusCode, message: String) -> Response {
    let code = match status {
        StatusCode::BAD_REQUEST => "invalid_request",
        StatusCode::UNAUTHORIZED => "unauthorized",
        StatusCode::FORBIDDEN => "forbidden",
        StatusCode::NOT_FOUND => "not_found",
        StatusCode::CONFLICT => "conflict",
        StatusCode::UNPROCESSABLE_ENTITY => "invalid_request",
        StatusCode::BAD_GATEWAY => "host_unavailable",
        StatusCode::SERVICE_UNAVAILABLE => "provider_unavailable",
        StatusCode::GATEWAY_TIMEOUT => "command_timeout",
        StatusCode::INTERNAL_SERVER_ERROR => "internal_error",
        _ => "invalid_request",
    };
    error_response_with_code(status, code, message)
}

fn error_response_with_code(status: StatusCode, code: &'static str, message: String) -> Response {
    (status, Json(json!({ "code": code, "message": message }))).into_response()
}

/// Body of a create-thread request.
///
/// The wire shape is bb's `createThreadRequestSchema`: camelCase, with
/// `projectId`, `origin`, `input` and `environment` required. `environment`
/// names where the workspace comes from, exactly as bb's UI sends it.
///
/// There is no snake_case fallback. W-554 decided loom accepts only the
/// contract shape, and `ui/src/main.ts` was updated in the same change; see
/// `docs/api-coverage.md` for why accepting both was rejected.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateThreadRequest {
    /// The owning project. **Required**: a thread must belong to a project.
    pub project_id: loom_domain::ProjectId,
    /// Where the thread came from. Required by the contract; loom records it
    /// on the thread-created event but does not otherwise branch on it yet.
    pub origin: CreateThreadOrigin,
    /// The initial prompt rows. bb requires the field; an empty list is how a
    /// client opens an empty thread.
    #[serde(default)]
    pub input: Vec<Value>,
    /// The execution context to bind, as bb's discriminated union. `reuse`
    /// binds an existing environment; `project-default` leaves the thread
    /// unbound (loom resolves a workspace at dispatch time).
    pub environment: CreateThreadEnvironment,
    /// Optional display title.
    #[serde(default)]
    pub title: Option<String>,
}

/// bb's `createThreadRequestSchema.origin`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CreateThreadOrigin {
    App,
    Cli,
    Sdk,
    Plugin,
}

/// bb's `createThreadRequestSchema.environment` discriminated union, narrowed
/// to the two variants loom can honour today.
#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum CreateThreadEnvironment {
    /// Reuse an existing environment.
    Reuse {
        #[serde(rename = "environmentId")]
        environment_id: EnvironmentId,
    },
    /// Let the project decide. loom leaves the thread unbound and resolves a
    /// workspace when the first turn dispatches.
    ProjectDefault,
}

impl CreateThreadRequest {
    /// The environment the thread should bind, if any.
    fn environment_id(&self) -> Option<EnvironmentId> {
        match &self.environment {
            CreateThreadEnvironment::Reuse { environment_id } => Some(environment_id.clone()),
            CreateThreadEnvironment::ProjectDefault => None,
        }
    }
}

/// Every known thread, projected into bb's `threadListEntrySchema` shape
/// (`$defs/d317`). The contract types list rows differently from the single
/// thread response, so the projection — not the domain type — is the payload.
///
/// The route returns a bare array (`$defs/d130`), not `{ threads }`.
async fn list_threads(State(state): State<AppState>) -> Json<Vec<Value>> {
    Json(
        state
            .registry
            .threads()
            .iter()
            .map(|thread| thread_list_entry_value(&state, thread))
            .collect(),
    )
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
struct ThreadGetQuery {
    include: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ThreadEventsQuery {
    after_seq: Option<String>,
    before_seq: Option<String>,
    limit: Option<String>,
    order: Option<String>,
    types: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
struct ThreadTimelineQuery {
    after_sequence: Option<String>,
    before_anchor_id: Option<String>,
    before_anchor_seq: Option<String>,
    include_nested_rows: Option<String>,
    segment_limit: Option<String>,
    summary_only: Option<String>,
}

#[allow(clippy::result_large_err)]
fn parse_thread_id(raw: &str) -> Result<ThreadId, Response> {
    raw.parse::<ThreadId>()
        .map_err(|error| error_response(StatusCode::BAD_REQUEST, error.to_string()))
}

#[allow(clippy::result_large_err)]
fn thread_domain_events(
    state: &AppState,
    thread_id: &ThreadId,
) -> Result<Vec<(String, u64, u64, DomainEvent)>, Response> {
    let scope = Scope::Thread(thread_id.to_string());
    let envelopes = state
        .relay
        .replay_scope(&scope, usize::MAX)
        .map_err(|error| error_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;
    Ok(envelopes
        .into_iter()
        .enumerate()
        .filter_map(|(index, envelope)| {
            domain_event_from_envelope(&envelope).map(|event| {
                (
                    envelope.event_id.to_string(),
                    index as u64 + 1,
                    envelope.created_at_ms,
                    event,
                )
            })
        })
        .collect())
}

#[allow(clippy::result_large_err)]
fn parse_query_sequence(raw: Option<&String>, field: &str) -> Result<Option<u64>, Response> {
    raw.filter(|value| !value.is_empty())
        .map(|value| {
            value.parse::<u64>().map_err(|error| {
                error_response(
                    StatusCode::BAD_REQUEST,
                    format!("invalid {field} query value {value:?}: {error}"),
                )
            })
        })
        .transpose()
}

fn thread_event_row(
    event_id: String,
    sequence: u64,
    created_at_ms: u64,
    run: &loom_domain::RunEvent,
) -> Value {
    let mut data = serde_json::to_value(&run.event.body).expect("ProviderEvent always serializes");
    if let Value::Object(object) = &mut data {
        object.remove("type");
    }
    json!({
        "id": event_id,
        "scope": run.event.scope,
        "threadId": run.event.thread_id.to_string(),
        "seq": sequence,
        "type": run.event.kind(),
        "data": data,
        "createdAt": created_at_ms
    })
}

/// Returns the provider's contract events from the thread room.
///
/// Domain lifecycle and message events deliberately stay out of this route:
/// bb's `threads.events` is a `ThreadEventRow[]`, projected from the inner
/// `run.event` of loom's domain wrapper.
async fn thread_events(
    State(state): State<AppState>,
    Path(raw_thread_id): Path<String>,
    Query(query): Query<ThreadEventsQuery>,
) -> Response {
    let thread_id = match parse_thread_id(&raw_thread_id) {
        Ok(thread_id) => thread_id,
        Err(response) => return response,
    };
    if state.registry.thread(&thread_id).is_none() {
        return error_response(
            StatusCode::NOT_FOUND,
            format!("thread {thread_id} is not known"),
        );
    }
    let after = match parse_query_sequence(query.after_seq.as_ref(), "afterSeq") {
        Ok(value) => value,
        Err(response) => return response,
    };
    let before = match parse_query_sequence(query.before_seq.as_ref(), "beforeSeq") {
        Ok(value) => value,
        Err(response) => return response,
    };
    let limit = match parse_query_sequence(query.limit.as_ref(), "limit") {
        Ok(Some(value)) => value.min(10_000) as usize,
        Ok(None) => 100,
        Err(response) => return response,
    };
    let descending = match query.order.as_deref() {
        None | Some("") | Some("asc") => false,
        Some("desc") => true,
        Some(value) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                format!("order must be asc or desc, got {value:?}"),
            )
        }
    };
    let types = query.types.as_deref().map(|value| {
        value
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .collect::<Vec<_>>()
    });

    let entries = match thread_domain_events(&state, &thread_id) {
        Ok(entries) => entries,
        Err(response) => return response,
    };
    let mut result = Vec::new();
    for (event_id, sequence, created_at_ms, event) in entries {
        let DomainEvent::ThreadRunEvent { run } = event else {
            continue;
        };
        if after.is_some_and(|value| sequence <= value)
            || before.is_some_and(|value| sequence >= value)
        {
            continue;
        }
        let event_type = run.event.kind();
        if types
            .as_ref()
            .is_some_and(|allowed| !allowed.contains(&event_type))
        {
            continue;
        }
        result.push(thread_event_row(event_id, sequence, created_at_ms, &run));
    }
    if descending {
        result.reverse();
    }
    result.truncate(limit);
    Json(result).into_response()
}

async fn get_thread(
    State(state): State<AppState>,
    Path(raw_thread_id): Path<String>,
    Query(_query): Query<ThreadGetQuery>,
) -> Response {
    let thread_id = match parse_thread_id(&raw_thread_id) {
        Ok(thread_id) => thread_id,
        Err(response) => return response,
    };
    match state.registry.thread(&thread_id) {
        Some(thread) => Json(thread_summary_value(&state, &thread)).into_response(),
        None => error_response(
            StatusCode::NOT_FOUND,
            format!("thread {thread_id} is not known"),
        ),
    }
}

async fn thread_output(
    State(state): State<AppState>,
    Path(raw_thread_id): Path<String>,
) -> Response {
    let thread_id = match parse_thread_id(&raw_thread_id) {
        Ok(thread_id) => thread_id,
        Err(response) => return response,
    };
    if state.registry.thread(&thread_id).is_none() {
        return error_response(
            StatusCode::NOT_FOUND,
            format!("thread {thread_id} is not known"),
        );
    }

    let mut delta_output = String::new();
    let mut saw_delta = false;
    let entries = match thread_domain_events(&state, &thread_id) {
        Ok(entries) => entries,
        Err(response) => return response,
    };
    let mut completed_output = None;
    for (_event_id, _sequence, _created_at_ms, event) in entries {
        let DomainEvent::ThreadRunEvent { run } = event else {
            continue;
        };
        let value = serde_json::to_value(&run.event).expect("ThreadEvent always serializes");
        match value.get("type").and_then(Value::as_str) {
            Some("item/agentMessage/delta") => {
                if let Some(delta) = value.get("delta").and_then(Value::as_str) {
                    saw_delta = true;
                    delta_output.push_str(delta);
                }
            }
            Some("item/completed")
                if value
                    .get("item")
                    .and_then(|item| item.get("type"))
                    .and_then(Value::as_str)
                    == Some("agentMessage") =>
            {
                completed_output = value
                    .get("item")
                    .and_then(|item| item.get("text"))
                    .and_then(Value::as_str)
                    .map(str::to_owned);
            }
            _ => {}
        }
    }
    let output = if saw_delta {
        Some(delta_output)
    } else {
        completed_output
    };
    Json(json!({ "output": output })).into_response()
}

async fn read_thread(State(state): State<AppState>, Path(raw_thread_id): Path<String>) -> Response {
    let thread_id = match parse_thread_id(&raw_thread_id) {
        Ok(thread_id) => thread_id,
        Err(response) => return response,
    };
    match state
        .registry
        .mark_thread_read(&thread_id, loom_relay::now_ms())
    {
        Ok(thread) => Json(thread_summary_value(&state, &thread)).into_response(),
        Err(error) => command_error_response(error),
    }
}

/// Body of a `threads.send` request.
///
/// The contract (`threads.send`) requires both fields, so they are not
/// `Option` here: a missing or wrongly-typed field is a serde rejection, which
/// `normalize_api_error` turns into the uniform API error body. `input` is
/// narrowed to bb's text variant because loom's execution plane has no other
/// prompt kind yet.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SendThreadRequest {
    input: Vec<SendInput>,
    mode: SendMode,
}

/// bb's `sendThreadRequestSchema.mode`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum SendMode {
    QueueIfActive,
    SteerIfActive,
    Auto,
    Start,
    Steer,
}

/// One row of bb's `sendThreadRequestSchema.input` (`$defs/d662`). loom reads
/// the text variant and refuses the rest, instead of silently dropping an
/// attachment it cannot deliver.
#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum SendInput {
    /// A plain text prompt.
    Text { text: String },
}

fn text_from_send_input(input: &[SendInput]) -> Option<String> {
    let mut text = String::new();
    for item in input {
        let SendInput::Text { text: value } = item;
        text.push_str(value);
    }
    (!text.trim().is_empty()).then_some(text)
}

/// Sends a bb prompt through the same registry -> publish -> dispatch path as
/// the compatibility `/messages` endpoint.
async fn send_thread(
    State(state): State<AppState>,
    Path(raw_thread_id): Path<String>,
    Json(request): Json<SendThreadRequest>,
) -> Response {
    let thread_id = match parse_thread_id(&raw_thread_id) {
        Ok(thread_id) => thread_id,
        Err(response) => return response,
    };
    let _mode = request.mode;
    let Some(content) = text_from_send_input(&request.input) else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "loom currently accepts text prompt inputs only".into(),
        );
    };
    match append_thread_message(&state, &thread_id, MessageRole::User, content) {
        Ok(_) => Json(json!({ "ok": true, "delivery": "sent" })).into_response(),
        Err(response) => response,
    }
}

async fn thread_tabs(State(state): State<AppState>, Path(raw_thread_id): Path<String>) -> Response {
    let thread_id = match parse_thread_id(&raw_thread_id) {
        Ok(thread_id) => thread_id,
        Err(response) => return response,
    };
    if state.registry.thread(&thread_id).is_none() {
        return error_response(
            StatusCode::NOT_FOUND,
            format!("thread {thread_id} is not known"),
        );
    }
    Json(json!({ "revision": 0, "tabs": [] })).into_response()
}

fn timeline_row_base(
    id: String,
    thread_id: &ThreadId,
    turn_id: Option<String>,
    sequence: u64,
    created_at_ms: u64,
) -> Value {
    json!({
        "id": id,
        "threadId": thread_id.to_string(),
        "turnId": turn_id,
        "sourceSeqStart": sequence,
        "sourceSeqEnd": sequence,
        "startedAt": created_at_ms,
        "createdAt": created_at_ms
    })
}

fn timeline_row_for_event(
    thread_id: &ThreadId,
    sequence: u64,
    created_at_ms: u64,
    event: &DomainEvent,
) -> Option<Value> {
    let mut base = match event {
        DomainEvent::ThreadMessageAdded { message, .. } => timeline_row_base(
            message.id.to_string(),
            thread_id,
            None,
            sequence,
            message.created_at_ms,
        ),
        DomainEvent::ThreadStatusChanged { .. } => timeline_row_base(
            format!("status-{sequence}"),
            thread_id,
            None,
            sequence,
            created_at_ms,
        ),
        DomainEvent::ThreadRunEvent { run } => timeline_row_base(
            format!("{}-{sequence}", run.run_id),
            thread_id,
            Some(run.run_id.to_string()),
            sequence,
            run.at_ms,
        ),
        _ => return None,
    };
    let object = base
        .as_object_mut()
        .expect("timeline row base is an object");
    match event {
        DomainEvent::ThreadMessageAdded { message, .. } => match message.role {
            MessageRole::User => {
                object.extend([
                    ("kind".into(), json!("conversation")),
                    ("text".into(), json!(message.content)),
                    ("attachments".into(), Value::Null),
                    ("role".into(), json!("user")),
                    ("initiator".into(), json!("user")),
                    ("senderThreadId".into(), Value::Null),
                    ("systemMessageKind".into(), json!("unlabeled")),
                    ("systemMessageSubject".into(), Value::Null),
                    (
                        "turnRequest".into(),
                        json!({ "isGrouped": false, "kind": "message", "status": "accepted" }),
                    ),
                    ("mentions".into(), json!([])),
                ]);
            }
            MessageRole::Assistant => {
                object.extend([
                    ("kind".into(), json!("conversation")),
                    ("text".into(), json!(message.content)),
                    ("attachments".into(), Value::Null),
                    ("role".into(), json!("assistant")),
                    ("turnRequest".into(), Value::Null),
                ]);
            }
            MessageRole::System => {
                object.extend([
                    ("kind".into(), json!("system")),
                    ("title".into(), json!("System message")),
                    ("detail".into(), json!(message.content)),
                    ("status".into(), Value::Null),
                    ("systemKind".into(), json!("debug")),
                ]);
            }
        },
        DomainEvent::ThreadStatusChanged { from, to, .. } => {
            object.extend([
                ("kind".into(), json!("system")),
                ("title".into(), json!("Thread status changed")),
                ("detail".into(), json!(format!("{from} -> {to}"))),
                ("status".into(), Value::Null),
                ("systemKind".into(), json!("debug")),
            ]);
        }
        DomainEvent::ThreadRunEvent { run } => {
            let event_value =
                serde_json::to_value(&run.event).expect("ThreadEvent always serializes");
            match event_value.get("type").and_then(Value::as_str) {
                Some("item/agentMessage/delta") => {
                    object.extend([
                        ("kind".into(), json!("conversation")),
                        (
                            "text".into(),
                            event_value
                                .get("delta")
                                .cloned()
                                .unwrap_or_else(|| json!("")),
                        ),
                        ("attachments".into(), Value::Null),
                        ("role".into(), json!("assistant")),
                        ("turnRequest".into(), Value::Null),
                    ]);
                }
                Some("item/completed")
                    if event_value
                        .get("item")
                        .and_then(|item| item.get("type"))
                        .and_then(Value::as_str)
                        == Some("agentMessage") =>
                {
                    object.extend([
                        ("kind".into(), json!("conversation")),
                        (
                            "text".into(),
                            event_value
                                .pointer("/item/text")
                                .cloned()
                                .unwrap_or_else(|| json!("")),
                        ),
                        ("attachments".into(), Value::Null),
                        ("role".into(), json!("assistant")),
                        ("turnRequest".into(), Value::Null),
                    ]);
                }
                _ => return None,
            }
        }
        _ => return None,
    }
    Some(base)
}

async fn thread_timeline(
    State(state): State<AppState>,
    Path(raw_thread_id): Path<String>,
    Query(query): Query<ThreadTimelineQuery>,
) -> Response {
    let thread_id = match parse_thread_id(&raw_thread_id) {
        Ok(thread_id) => thread_id,
        Err(response) => return response,
    };
    if state.registry.thread(&thread_id).is_none() {
        return error_response(
            StatusCode::NOT_FOUND,
            format!("thread {thread_id} is not known"),
        );
    }
    let after = match parse_query_sequence(query.after_sequence.as_ref(), "afterSequence") {
        Ok(value) => value,
        Err(response) => return response,
    };
    let before_sequence =
        match parse_query_sequence(query.before_anchor_seq.as_ref(), "beforeAnchorSeq") {
            Ok(value) => value,
            Err(response) => return response,
        };
    let segment_limit = match parse_query_sequence(query.segment_limit.as_ref(), "segmentLimit") {
        Ok(Some(value)) => value.clamp(1, 1_000) as usize,
        Ok(None) => 100,
        Err(response) => return response,
    };

    let entries = match thread_domain_events(&state, &thread_id) {
        Ok(entries) => entries,
        Err(response) => return response,
    };
    let all_rows = entries
        .iter()
        .filter_map(|(_event_id, sequence, created_at_ms, event)| {
            timeline_row_for_event(&thread_id, *sequence, *created_at_ms, event)
        })
        .collect::<Vec<_>>();
    let before_id_sequence = query.before_anchor_id.as_ref().and_then(|anchor_id| {
        all_rows.iter().find_map(|row| {
            (row.get("id").and_then(Value::as_str) == Some(anchor_id.as_str()))
                .then(|| row.get("sourceSeqStart").and_then(Value::as_u64))
                .flatten()
        })
    });
    let before = before_sequence.or(before_id_sequence);
    let mut candidates = all_rows
        .into_iter()
        .filter(|row| {
            let sequence = row
                .get("sourceSeqEnd")
                .and_then(Value::as_u64)
                .unwrap_or_default();
            after.map_or(true, |value| sequence > value)
                && before.map_or(true, |value| sequence < value)
        })
        .collect::<Vec<_>>();
    let has_older_rows = candidates.len() > segment_limit;
    if candidates.len() > segment_limit {
        let start = candidates.len() - segment_limit;
        candidates = candidates.split_off(start);
    }
    let older_cursor = has_older_rows.then(|| {
        let first = candidates
            .first()
            .expect("limited timeline has a first row");
        json!({
            "anchorSeq": first["sourceSeqStart"],
            "anchorId": first["id"]
        })
    });
    let max_seq = entries
        .last()
        .map(|(_, sequence, _, _)| *sequence)
        .unwrap_or(0);
    Json(json!({
        "rows": candidates,
        "contextBoundarySeq": null,
        "activePromptMode": null,
        "activeThinking": null,
        "activeWorkflows": [],
        "activeBackgroundCommands": [],
        "pendingTodos": null,
        "goal": null,
        "modelFallback": null,
        "timelinePage": {
            "kind": if before.is_some() { "older" } else { "latest" },
            "segmentLimit": segment_limit,
            "returnedSegmentCount": candidates.len(),
            "hasOlderRows": has_older_rows,
            "olderCursor": older_cursor
        },
        "maxSeq": max_seq
    }))
    .into_response()
}

/// Creates a thread and returns it in bb's `threadSchema` shape (`$defs/d7`)
/// with the contract's `201`.
///
/// `projectId` is required. The event goes to the project scope, not the
/// (brand new, unsubscribable) thread scope: it is the project's thread list
/// that has to learn about it. The event id is not repeated in the body — the
/// contract does not type it, and a subscriber learns about the event on the
/// project scope — so the response is exactly the created thread.
async fn create_thread(
    State(state): State<AppState>,
    Json(request): Json<CreateThreadRequest>,
) -> Response {
    match state.registry.create_thread(
        Some(request.project_id.clone()),
        request.title.clone(),
        request.environment_id(),
        loom_relay::now_ms(),
    ) {
        Ok((thread, event)) => match state.publish_domain_event(&event) {
            Ok(_) => (
                StatusCode::CREATED,
                Json(thread_summary_value(&state, &thread)),
            )
                .into_response(),
            Err(error) => error_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
        },
        Err(error) => command_error_response(error),
    }
}

/// Body of a post-message request.
#[derive(Clone, Debug, Deserialize)]
pub struct PostMessageRequest {
    /// The message body.
    pub content: String,
    /// Who produced it. Defaults to the user.
    #[serde(default)]
    pub role: MessageRole,
}

/// One event a command published.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct PublishedEvent {
    /// The event id, usable as a resume cursor.
    pub event_id: String,
    /// The stable event type tag.
    pub event_type: &'static str,
    /// The scope it was published to.
    pub scope: DomainScope,
}

/// The events a message produced.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct PostMessageResponse {
    /// The thread the message landed in.
    pub thread_id: ThreadId,
    /// Events in publication order: the message, then any status change.
    pub events: Vec<PublishedEvent>,
}

/// Appends a message to a thread.
///
/// A user message into an `idle` thread also starts a run, so this commonly
/// publishes two events to the thread scope. They are published in order, so a
/// subscriber sees the message and then the status change.
async fn post_thread_message(
    State(state): State<AppState>,
    Path(raw_thread_id): Path<String>,
    Json(request): Json<PostMessageRequest>,
) -> Response {
    let thread_id = match raw_thread_id.parse::<ThreadId>() {
        Ok(thread_id) => thread_id,
        Err(error) => return error_response(StatusCode::BAD_REQUEST, error.to_string()),
    };
    let content = request.content;
    match append_thread_message(&state, &thread_id, request.role, content) {
        Ok(published) => Json(PostMessageResponse {
            thread_id,
            events: published,
        })
        .into_response(),
        Err(response) => response,
    }
}

/// Appends a message, publishes its domain events and dispatches a newly
/// started user turn through the existing relay path.
#[allow(clippy::result_large_err)]
fn append_thread_message(
    state: &AppState,
    thread_id: &ThreadId,
    role: MessageRole,
    content: String,
) -> Result<Vec<PublishedEvent>, Response> {
    let events = state
        .registry
        .post_message(thread_id, role, content.clone(), loom_relay::now_ms())
        .map_err(command_error_response)?;
    let published = publish_all(state, &events)
        .map_err(|error| error_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;

    // A user message into an idle thread moves it to `working`. That is the
    // trigger for dispatch: find a machine and publish a run to its scope
    // through the relay. If no machine exists the dispatcher fails the thread
    // on the spot, so the status change is never left dangling.
    if published
        .iter()
        .any(|event| event.event_type == "thread_status_changed")
    {
        if let Some(thread) = state.registry.thread(thread_id) {
            state.dispatch_thread(&thread, &content);
        }
    }
    Ok(published)
}

/// Provider runs currently dispatched and not yet terminal.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct RunListResponse {
    /// In-flight runs, in run-id order.
    pub runs: Vec<crate::runs::RunRecord>,
}

/// Lists in-flight runs. Useful for operators and for tests asserting that a
/// run is reaped rather than left hanging.
async fn list_runs(State(state): State<AppState>) -> Json<RunListResponse> {
    Json(RunListResponse {
        runs: state.runs.all(),
    })
}

/// Body of a host-registration request.
#[derive(Clone, Debug, Deserialize)]
pub struct RegisterHostRequest {
    /// A daemon-chosen identity, so a reconnect updates the same machine
    /// instead of creating a second one. Omitted mints a fresh id.
    #[serde(default)]
    pub id: Option<HostId>,
    /// The machine's display name.
    pub name: String,
}

/// A registered host and the event that announced it.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct RegisterHostResponse {
    /// The host, in status `connected`.
    pub host: Host,
    /// The `host_registered` event, published to `host:{id}`. Empty when a
    /// reconnect changed nothing.
    pub event_id: String,
}

/// Registers or re-enrolls a host.
///
/// Idempotent when the body carries an `id`: a daemon that reconnects under
/// the identity it was given produces a status change, not a new machine.
async fn register_host(
    State(state): State<AppState>,
    Json(request): Json<RegisterHostRequest>,
) -> Response {
    match state
        .registry
        .enroll_host(request.id, request.name, loom_relay::now_ms())
    {
        Ok((host, events)) => match publish_all(&state, &events) {
            Ok(published) => Json(RegisterHostResponse {
                host,
                event_id: published
                    .last()
                    .map(|event| event.event_id.clone())
                    .unwrap_or_default(),
            })
            .into_response(),
            Err(error) => error_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
        },
        Err(error) => command_error_response(error),
    }
}

async fn list_hosts(State(state): State<AppState>) -> Json<Vec<Value>> {
    Json(state.registry.hosts().iter().map(host_value).collect())
}

/// Records a daemon heartbeat. Heartbeats are high frequency and deliberately
/// do not publish a frame.
async fn host_heartbeat(
    State(state): State<AppState>,
    Path(raw_host_id): Path<String>,
) -> Response {
    let host_id = match raw_host_id.parse::<HostId>() {
        Ok(host_id) => host_id,
        Err(error) => return error_response(StatusCode::BAD_REQUEST, error.to_string()),
    };
    match state
        .registry
        .host_heartbeat(&host_id, loom_relay::now_ms())
    {
        Ok(host) => Json(host).into_response(),
        Err(error) => command_error_response(error),
    }
}

/// Marks a host's daemon detached without closing anything on the server.
async fn disconnect_host(
    State(state): State<AppState>,
    Path(raw_host_id): Path<String>,
) -> Response {
    let host_id = match raw_host_id.parse::<HostId>() {
        Ok(host_id) => host_id,
        Err(error) => return error_response(StatusCode::BAD_REQUEST, error.to_string()),
    };
    match state
        .registry
        .mark_host_disconnected(&host_id, loom_relay::now_ms())
    {
        Ok(events) => match publish_all(&state, &events) {
            Ok(_) => Json(serde_json::json!({ "host_id": host_id })).into_response(),
            Err(error) => error_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
        },
        Err(error) => command_error_response(error),
    }
}

/// Body of a create-project request, in bb's `createProjectRequestSchema`
/// shape: a name plus where the code lives (`source`).
#[derive(Clone, Debug, Deserialize)]
pub struct CreateProjectRequest {
    /// Display name. Must not be blank.
    pub name: String,
    /// Where the project's code lives. Required by the contract; loom records
    /// it as the project's first source.
    pub source: CreateProjectSource,
}

/// bb's project-source discriminated union, narrowed to the variants loom can
/// honour today. `local_path` is a plain location; `clone` records a remote
/// before any checkout exists.
#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CreateProjectSource {
    /// An existing directory on a host.
    LocalPath {
        #[serde(rename = "hostId")]
        host_id: HostId,
        path: String,
    },
    /// A repository to clone. loom records the remote; it never clones here.
    Clone {
        #[serde(rename = "hostId")]
        host_id: HostId,
        #[serde(default, rename = "remoteUrl")]
        remote_url: Option<String>,
        #[serde(default, rename = "targetPath")]
        target_path: Option<String>,
    },
}

impl CreateProjectSource {
    /// The host the source lives on.
    fn host_id(&self) -> HostId {
        match self {
            Self::LocalPath { host_id, .. } | Self::Clone { host_id, .. } => host_id.clone(),
        }
    }

    /// The absolute path, empty for a remote-only clone.
    fn path(&self) -> String {
        match self {
            Self::LocalPath { path, .. } => path.clone(),
            Self::Clone { target_path, .. } => target_path.clone().unwrap_or_default(),
        }
    }

    /// The git remote, when the source is a checkout.
    fn remote_url(&self) -> Option<String> {
        match self {
            Self::LocalPath { .. } => None,
            Self::Clone { remote_url, .. } => remote_url.clone(),
        }
    }
}

/// A project after a mutation, in bb's `projectSchema` shape (`$defs/d627`).
///
/// A single project, not an envelope: the contract types the response body as
/// the project and `additionalProperties: false` means an `event_id` beside it
/// is a rejection. A client that wants the event id subscribes to the scope;
/// that is what the relay is for.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct ProjectResponse {
    /// The project after the change.
    pub project: Value,
}

/// Lists projects.
///
/// `project_created` is published to `global`, because the project list is not
/// scoped to a project a client can already have subscribed to. A client
/// therefore subscribes to `global` once and follows the list from there.
///
/// The contract returns a bare array of `projectSchema` (`$defs/d471`).
async fn list_projects(State(state): State<AppState>) -> Json<Vec<Value>> {
    Json(
        state
            .registry
            .projects()
            .iter()
            .map(project_value)
            .collect(),
    )
}

/// Creates a project and its first source.
///
async fn create_project(
    State(state): State<AppState>,
    Json(request): Json<CreateProjectRequest>,
) -> Response {
    let source = request.source;
    let (project, event) = match state.registry.create_project(
        request.name,
        ProjectKind::Standard,
        source.remote_url(),
        loom_relay::now_ms(),
    ) {
        Ok(result) => result,
        Err(error) => return command_error_response(error),
    };
    // The contract pairs creation with its initial location. loom records the
    // source as a second event rather than guessing a default later; a host
    // that was never enrolled is a `404` (the same rule `projects.createSource`
    // uses).
    match state.registry.add_project_source(
        &project.id,
        source.host_id(),
        source.path(),
        source.remote_url(),
        loom_relay::now_ms(),
    ) {
        Ok((project, _)) => match state.publish_domain_event(&event) {
            Ok(_) => (StatusCode::CREATED, Json(project_value(&project))).into_response(),
            Err(error) => error_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
        },
        Err(error) => command_error_response(error),
    }
}

/// Fetches one project by id.
async fn get_project(
    State(state): State<AppState>,
    Path(raw_project_id): Path<String>,
) -> Response {
    let project_id = match raw_project_id.parse::<ProjectId>() {
        Ok(project_id) => project_id,
        Err(error) => return error_response(StatusCode::BAD_REQUEST, error.to_string()),
    };
    match state.registry.project(&project_id) {
        Some(project) => Json(project_value(&project)).into_response(),
        None => error_response(
            StatusCode::NOT_FOUND,
            format!("project {project_id} is not known"),
        ),
    }
}

/// Body of a project update, in bb's `updateProjectRequestSchema` shape: an
/// optional new name. At least one field is required, which for loom means
/// `name` must be present.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateProjectRequest {
    /// A new display name.
    #[serde(default)]
    pub name: Option<String>,
}

/// Renames a project and/or changes its remote.
///
/// The `project_updated` event goes to `project:{id}`, where a client watching
/// that project's list state sees it.
async fn update_project(
    State(state): State<AppState>,
    Path(raw_project_id): Path<String>,
    Json(request): Json<UpdateProjectRequest>,
) -> Response {
    let project_id = match raw_project_id.parse::<ProjectId>() {
        Ok(project_id) => project_id,
        Err(error) => return error_response(StatusCode::BAD_REQUEST, error.to_string()),
    };
    if request.name.is_none() {
        return error_response(StatusCode::BAD_REQUEST, "provide a name".into());
    }

    let now = loom_relay::now_ms();
    let mut events = Vec::new();
    let mut project = match state.registry.project(&project_id) {
        Some(project) => project,
        None => {
            return error_response(
                StatusCode::NOT_FOUND,
                format!("project {project_id} is not known"),
            )
        }
    };
    if let Some(name) = request.name {
        match state.registry.rename_project(&project_id, name, now) {
            Ok((updated, event)) => {
                project = updated;
                events.push(event);
            }
            Err(error) => return command_error_response(error),
        }
    }

    match publish_all(&state, &events) {
        Ok(published) => {
            let _ = published;
            Json(project_value(&project)).into_response()
        }
        Err(error) => error_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    }
}

/// Archives a project.
///
/// Refuses a project with a run in flight (`working`/`waiting` thread); idle
/// threads are not cascaded and keep their project reference. Archiving is
/// terminal for the record, so a second call is a conflict.
///
async fn archive_project(
    State(state): State<AppState>,
    Path(raw_project_id): Path<String>,
) -> Response {
    let project_id = match raw_project_id.parse::<ProjectId>() {
        Ok(project_id) => project_id,
        Err(error) => return error_response(StatusCode::BAD_REQUEST, error.to_string()),
    };
    match state
        .registry
        .archive_project(&project_id, loom_relay::now_ms())
    {
        Ok((project, event)) => match publish_all(&state, &[event]) {
            Ok(_) => Json(project).into_response(),
            Err(error) => error_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
        },
        Err(error) => command_error_response(error),
    }
}

/// Body of an add-source request, in bb's `createProjectSourceRequestSchema`
/// shape. `hostId` and `type` are required by the contract; the host is never
/// defaulted, so a source is never written against a guess.
pub use CreateProjectSource as AddProjectSourceRequest;

/// Adds a source to a project.
///
/// Declaring a source never clones or fetches: it records where the code is (or
/// will be) on a host. Materialising a workspace is environment provisioning's
/// job.
async fn add_project_source(
    State(state): State<AppState>,
    Path(raw_project_id): Path<String>,
    Json(request): Json<AddProjectSourceRequest>,
) -> Response {
    let project_id = match raw_project_id.parse::<ProjectId>() {
        Ok(project_id) => project_id,
        Err(error) => return error_response(StatusCode::BAD_REQUEST, error.to_string()),
    };
    let host_id = request.host_id();
    let path = request.path();
    let remote_url = request.remote_url();
    match state.registry.add_project_source(
        &project_id,
        host_id,
        path,
        remote_url,
        loom_relay::now_ms(),
    ) {
        Ok((project, event)) => {
            let source = project
                .sources
                .last()
                .cloned()
                .expect("add_source always appends one");
            match state.publish_domain_event(&event) {
                Ok(_) => (StatusCode::CREATED, Json(project_source_value(&source))).into_response(),
                Err(error) => error_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
            }
        }
        Err(error) => command_error_response(error),
    }
}

/// Removes a source from a project.
async fn remove_project_source(
    State(state): State<AppState>,
    Path((raw_project_id, raw_source_id)): Path<(String, String)>,
) -> Response {
    let project_id = match raw_project_id.parse::<ProjectId>() {
        Ok(project_id) => project_id,
        Err(error) => return error_response(StatusCode::BAD_REQUEST, error.to_string()),
    };
    let source_id = match raw_source_id.parse::<ProjectSourceId>() {
        Ok(source_id) => source_id,
        Err(error) => return error_response(StatusCode::BAD_REQUEST, error.to_string()),
    };
    match state
        .registry
        .remove_project_source(&project_id, &source_id, loom_relay::now_ms())
    {
        Ok(Some((_project, event))) => match state.publish_domain_event(&event) {
            Ok(_) => Json(json!({ "ok": true })).into_response(),
            Err(error) => error_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
        },
        Ok(None) => error_response(
            StatusCode::NOT_FOUND,
            format!("source {source_id} is not part of project {project_id}"),
        ),
        Err(error) => command_error_response(error),
    }
}

/// Body of a create-environment request.
#[derive(Clone, Debug, Deserialize)]
pub struct CreateEnvironmentRequest {
    /// `managed` (loom creates the directory) or `unmanaged` (an existing one).
    pub kind: EnvironmentKind,
    /// The owning project. **Required**: an environment belongs to a project,
    /// and the server no longer defaults to the seeded personal one.
    #[serde(default)]
    pub project_id: Option<ProjectId>,
    /// The host the workspace lives on. Omitted uses the primary host.
    #[serde(default)]
    pub host_id: Option<HostId>,
    /// Absolute path for an unmanaged environment. Must be absent for a managed
    /// one, whose path is decided by the daemon that provisions it.
    #[serde(default)]
    pub path: Option<String>,
}

/// A created environment and the event that announced it.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct CreateEnvironmentResponse {
    /// The environment as created: `ready` for an unmanaged one, `creating` for
    /// a managed one. A managed environment's follow-up status changes arrive
    /// on the project scope.
    pub environment: Environment,
    /// The `environment_created` event, published to `project:{project_id}`.
    pub event_id: String,
}

/// Every known environment, optionally filtered to one project.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct EnvironmentListResponse {
    /// Environments, oldest first, in bb's `environmentSchema` shape.
    pub environments: Vec<Value>,
}

/// Query for [`list_environments`].
#[derive(Clone, Debug, Deserialize)]
pub struct EnvironmentListQuery {
    /// Filter to one project.
    #[serde(default)]
    pub project_id: Option<String>,
}

/// Creates an environment.
///
/// An unmanaged environment is usable immediately: it must name an absolute
/// path and starts `ready`. A managed one starts `creating`, and provisioning
/// is dispatched to its host in the same request; if the daemon is not there
/// yet the request is retained in the host room and replayed on reconnect.
///
/// The path's *existence* is deliberately **not** checked here. The path is on
/// the host's filesystem, which may be another machine, so the daemon is the
/// only party that can validate it (and it does, refusing to start a provider
/// when the directory is missing).
async fn create_environment(
    State(state): State<AppState>,
    Json(request): Json<CreateEnvironmentRequest>,
) -> Response {
    let host_id = match request.host_id.clone().or_else(|| {
        state
            .registry
            .primary_host(state.local_host_id())
            .map(|host| host.id)
    }) {
        Some(host_id) => host_id,
        None => {
            return error_response(
                StatusCode::CONFLICT,
                "no host is available to own the environment; enroll a daemon first".into(),
            )
        }
    };

    if request.kind == EnvironmentKind::Unmanaged {
        match request.path.as_deref() {
            None => {
                return error_response(
                    StatusCode::BAD_REQUEST,
                    "an unmanaged environment requires a path".into(),
                )
            }
            Some(path) if !std::path::Path::new(path).is_absolute() => {
                return error_response(
                    StatusCode::BAD_REQUEST,
                    format!("path {path:?} must be absolute"),
                )
            }
            Some(_) => {}
        }
    }

    let (environment, events) = match state.registry.create_environment(
        request.project_id,
        host_id,
        request.kind,
        request.path,
        loom_relay::now_ms(),
    ) {
        Ok(result) => result,
        Err(error) => return command_error_response(error),
    };
    let published = match publish_all(&state, &events) {
        Ok(published) => published,
        Err(error) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    };

    // A managed environment is provisioned right away. The response still shows
    // the state just created (`creating`), which is the fact this call
    // produced; provisioning is an announced follow-up a client watches on the
    // project scope. Only a rejected append is reflected, because then the
    // environment really has moved to `error`.
    let environment = match state.provision_environment(&environment.id) {
        crate::environments::ProvisionOutcome::PublishFailed {
            environment: failed,
            ..
        } => failed,
        _ => environment,
    };

    Json(CreateEnvironmentResponse {
        environment,
        event_id: published
            .last()
            .map(|event| event.event_id.clone())
            .unwrap_or_default(),
    })
    .into_response()
}

/// Lists environments, optionally filtered by project.
async fn list_environments(
    State(state): State<AppState>,
    axum::extract::Query(query): axum::extract::Query<EnvironmentListQuery>,
) -> Response {
    let environments = match query.project_id.as_deref() {
        None | Some("") => state.registry.environments(),
        Some(raw) => match raw.parse::<ProjectId>() {
            Ok(project_id) => state.registry.environments_for_project(&project_id),
            Err(error) => return error_response(StatusCode::BAD_REQUEST, error.to_string()),
        },
    };
    // The contract returns a bare array of `environmentSchema` (`$defs/d52`),
    // and `projects.list`'s filter plus the CLI mean the envelope buys nothing.
    Json(
        environments
            .iter()
            .map(environment_value)
            .collect::<Vec<_>>(),
    )
    .into_response()
}

/// Fetches one environment by id.
async fn get_environment(
    State(state): State<AppState>,
    Path(raw_environment_id): Path<String>,
) -> Response {
    let environment_id = match raw_environment_id.parse::<EnvironmentId>() {
        Ok(environment_id) => environment_id,
        Err(error) => return error_response(StatusCode::BAD_REQUEST, error.to_string()),
    };
    match state.registry.environment(&environment_id) {
        Some(environment) => Json(environment_value(&environment)).into_response(),
        None => error_response(
            StatusCode::NOT_FOUND,
            format!("environment {environment_id} is not known"),
        ),
    }
}

/// (Re)dispatches provisioning for a managed environment.
///
async fn provision_environment(
    State(state): State<AppState>,
    Path(raw_environment_id): Path<String>,
) -> Response {
    let environment_id = match raw_environment_id.parse::<EnvironmentId>() {
        Ok(environment_id) => environment_id,
        Err(error) => return error_response(StatusCode::BAD_REQUEST, error.to_string()),
    };
    match state.provision_environment(&environment_id) {
        crate::environments::ProvisionOutcome::Dispatched(environment) => {
            Json(environment).into_response()
        }
        crate::environments::ProvisionOutcome::Unknown => error_response(
            StatusCode::NOT_FOUND,
            format!("environment {environment_id} is not known"),
        ),
        crate::environments::ProvisionOutcome::NotProvisionable {
            environment,
            reason,
        } => {
            if environment.status == EnvironmentStatus::Destroyed {
                command_error_response(CommandError::Domain(
                    DomainError::IllegalEnvironmentTransition {
                        from: environment.status,
                        to: EnvironmentStatus::Provisioning,
                    },
                ))
            } else {
                error_response(StatusCode::CONFLICT, reason)
            }
        }
        crate::environments::ProvisionOutcome::PublishFailed { environment, error } => {
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!(
                    "could not dispatch provisioning: {error}; environment is {}",
                    environment.status
                ),
            )
        }
    }
}

/// Destroys an environment; the transition is terminal.
///
/// This moves the record to `destroyed`. Removing the directory of a *managed*
/// environment is a separate concern (worktree teardown, tracked by its own
/// issue); an unmanaged directory is never touched by loom.
///
async fn destroy_environment(
    State(state): State<AppState>,
    Path(raw_environment_id): Path<String>,
) -> Response {
    let environment_id = match raw_environment_id.parse::<EnvironmentId>() {
        Ok(environment_id) => environment_id,
        Err(error) => return error_response(StatusCode::BAD_REQUEST, error.to_string()),
    };
    match state.registry.set_environment_status(
        &environment_id,
        EnvironmentStatus::Destroyed,
        loom_relay::now_ms(),
    ) {
        Ok((environment, event)) => match publish_all(&state, &[event]) {
            Ok(_) => Json(environment).into_response(),
            Err(error) => error_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
        },
        Err(error) => command_error_response(error),
    }
}

/// Where the primary host came from.
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PrimaryHostSource {
    /// The operator-declared local host, which is connected.
    Local,
    /// A connected execution machine that is not the server's own machine.
    Remote,
    /// No host is enrolled and connected. A normal state, not an error.
    NoHost,
}

/// The host file browsing and usage queries should use.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct PrimaryHostResponse {
    /// The chosen host, or `null` when none is available.
    pub host: Option<Host>,
    /// Why that host, so a client can render the degradation honestly.
    pub source: PrimaryHostSource,
}

/// Resolves the primary host.
///
/// Always `200`. The one property this route exists to guarantee: a machine
/// with no local daemon must not make file browsing or host lookups fail. The
/// local host is a preference only, and its absence degrades to a remote host
/// or to an explicit `no_host` — never to `host_unavailable`.
async fn primary_host(State(state): State<AppState>) -> Json<PrimaryHostResponse> {
    let local = state.local_host_id();
    let host = state.registry.primary_host(local);
    let source = match &host {
        None => PrimaryHostSource::NoHost,
        Some(host) if Some(&host.id) == local => PrimaryHostSource::Local,
        Some(_) => PrimaryHostSource::Remote,
    };
    Json(PrimaryHostResponse { host, source })
}

/// Publishes each domain event to the scope the domain assigned it, in order.
fn publish_all(
    state: &AppState,
    events: &[DomainEvent],
) -> loom_relay::Result<Vec<PublishedEvent>> {
    events
        .iter()
        .map(|event| {
            let envelope = state.publish_domain_event(event)?;
            Ok(PublishedEvent {
                event_id: envelope.event_id.to_string(),
                event_type: event.kind(),
                scope: event.scope(),
            })
        })
        .collect()
}

/// Maps a command failure onto an HTTP status.
fn command_error_response(error: CommandError) -> Response {
    match error {
        CommandError::NotFound(message) => error_response(StatusCode::NOT_FOUND, message),
        CommandError::Conflict(message) => error_response(StatusCode::CONFLICT, message),
        CommandError::Domain(
            error @ (DomainError::InvalidField { .. } | DomainError::MalformedId { .. }),
        ) => error_response(StatusCode::BAD_REQUEST, error.to_string()),
        CommandError::Domain(error) => error_response(StatusCode::CONFLICT, error.to_string()),
    }
}

/// Query for [`replay`].
#[derive(Clone, Debug, Deserialize)]
pub struct ReplayQuery {
    /// Scope kind, one of `global`, `project`, `thread`, `host`, `client`,
    /// `user`.
    pub scope_kind: String,
    /// Scope id. Omitted only for `global`.
    pub scope_id: Option<String>,
    /// Maximum frames per response. Defaults to 100.
    pub limit: Option<usize>,
    /// Page forward from this event id, exclusively.
    ///
    /// With a cursor the response is the **oldest** frames after it, and
    /// [`ReplayResponse::has_more`] says whether another page exists. A client
    /// resuming must keep paging while `has_more`, otherwise it advances past a
    /// gap it can no longer recover. Without a cursor the response is the
    /// **newest** frames in the retention window and `has_more` is always
    /// `false`.
    pub since: Option<String>,
}

/// Response for [`replay`].
///
/// Frames are returned exactly as the socket would have delivered them, so a
/// client merges backlog and live traffic by event id with no shape
/// translation.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct ReplayResponse {
    /// Client frames in ascending event-id order.
    pub frames: Vec<serde_json::Value>,
    /// Whether more frames exist after this page. Only ever `true` when a
    /// cursor was supplied.
    pub has_more: bool,
}

/// Replays retained frames for a scope.
///
/// Correct client flow is: subscribe on the socket **first**, then call this,
/// paging while `has_more`. Any frame that arrives live in the meantime is also
/// present here (or is newer than the window), and because every frame carries
/// an [`EventId`] the client drops the duplicate. Fetching first would instead
/// risk missing a frame published between the two calls.
///
/// [`EventId`]: loom_relay::EventId
async fn replay(
    State(state): State<AppState>,
    axum::extract::Query(query): axum::extract::Query<ReplayQuery>,
) -> Response {
    let scope = match parse_scope(&query.scope_kind, query.scope_id.as_deref()) {
        Ok(scope) => scope,
        Err(message) => return error_response(StatusCode::BAD_REQUEST, message),
    };

    let since = match query.since.as_deref() {
        None | Some("") => None,
        Some(raw) => match raw.parse::<loom_relay::EventId>() {
            Ok(id) => Some(id),
            Err(error) => {
                return error_response(
                    StatusCode::BAD_REQUEST,
                    format!("invalid since cursor: {error}"),
                )
            }
        },
    };

    let limit = query.limit.unwrap_or(100).clamp(1, 10_000);
    let (events, has_more) = match since {
        Some(since) => {
            match state
                .relay
                .replay_page_after(&scope, Some(since), loom_relay::now_ms(), limit)
            {
                Ok(page) => (page.events, page.has_more),
                Err(error) => {
                    return error_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
                }
            }
        }
        None => match state.relay.replay_scope(&scope, limit) {
            Ok(events) => (events, false),
            Err(error) => {
                return error_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
            }
        },
    };

    Json(ReplayResponse {
        frames: events
            .into_iter()
            .map(|envelope| {
                // Stored frames are JSON by construction; fall back to a JSON
                // string so an unexpected payload still replays losslessly.
                serde_json::from_slice(&envelope.payload).unwrap_or_else(|_| {
                    serde_json::Value::String(
                        String::from_utf8_lossy(&envelope.payload).into_owned(),
                    )
                })
            })
            .collect(),
        has_more,
    })
    .into_response()
}

fn parse_scope(kind: &str, id: Option<&str>) -> Result<Scope, String> {
    let required = |id: Option<&str>| {
        id.filter(|value| !value.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| format!("scope id is required for kind \"{kind}\""))
    };
    match kind {
        "global" => Ok(Scope::Global),
        "project" => required(id).map(Scope::Project),
        "thread" => required(id).map(Scope::Thread),
        "host" => required(id).map(Scope::Host),
        "client" => required(id).map(Scope::Client),
        "user" => required(id).map(Scope::User),
        other => Err(format!("unknown scope kind \"{other}\"")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::AppConfig;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn test_state() -> AppState {
        AppState::build(Default::default()).unwrap()
    }

    async fn body_json(response: Response) -> serde_json::Value {
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    async fn post(app: &Router, path: &str, body: serde_json::Value) -> Response {
        app.clone()
            .oneshot(
                Request::post(path)
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    async fn get(app: &Router, path: &str) -> Response {
        app.clone()
            .oneshot(Request::get(path).body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    fn assert_b1_response(
        contract: &loom_contract::Contract,
        id: &str,
        method: &str,
        body: &serde_json::Value,
    ) {
        assert_b1_response_status(contract, id, method, 200, body)
    }

    fn assert_b1_response_status(
        contract: &loom_contract::Contract,
        id: &str,
        method: &str,
        status: u16,
        body: &serde_json::Value,
    ) {
        let route = contract
            .route_by_id(id)
            .unwrap_or_else(|| panic!("missing contract route {id}"));
        assert_eq!(route.method, method, "wrong method in contract for {id}");
        let violations = contract.validate_response(route, status, body);
        assert!(
            violations.is_empty(),
            "{id} response does not conform at {status}: {violations:?}\n{body}"
        );

        let violations = contract.validate_response(route, status, &serde_json::Value::Null);
        assert!(
            !violations.is_empty(),
            "{id} response validator accepted a null counterexample"
        );
    }

    async fn assert_b1_get(
        contract: &loom_contract::Contract,
        app: &Router,
        id: &str,
        path: &str,
    ) -> serde_json::Value {
        let response = get(app, path).await;
        assert_eq!(response.status(), StatusCode::OK, "GET {path}");
        let body = body_json(response).await;
        assert_b1_response(contract, id, "GET", &body);
        body
    }

    async fn patch(app: &Router, path: &str, body: serde_json::Value) -> Response {
        app.clone()
            .oneshot(
                Request::patch(path)
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    async fn delete(app: &Router, path: &str) -> Response {
        app.clone()
            .oneshot(Request::delete(path).body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    /// Reads back the domain event stored in a scope's last frame.
    fn stored_events(state: &AppState, scope: &Scope) -> Vec<serde_json::Value> {
        state
            .relay
            .replay_scope(scope, 100)
            .unwrap()
            .into_iter()
            .map(|envelope| {
                let frame: serde_json::Value = serde_json::from_slice(&envelope.payload).unwrap();
                assert_eq!(frame["type"], "event");
                serde_json::from_str(frame["payload"].as_str().unwrap()).unwrap()
            })
            .collect()
    }

    #[tokio::test]
    async fn health_reports_the_fixed_reader_count() {
        let app = router(test_state());
        let response = app
            .oneshot(Request::get("/health").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let json = body_json(response).await;
        assert_eq!(json["status"], "ok");
        assert_eq!(json["readers"], usize::from(loom_relay::SHARD_COUNT));
        assert_eq!(json["protocol_version"], PROTOCOL_VERSION);
        // A healthy backend omits the field rather than reporting null.
        assert!(json.get("backend_error").is_none());
    }

    #[tokio::test]
    async fn version_reports_the_crate_version() {
        let app = router(test_state());
        let response = app
            .oneshot(Request::get("/api/v1/version").body(Body::empty()).unwrap())
            .await
            .unwrap();

        let json = body_json(response).await;
        assert_eq!(json["version"], env!("CARGO_PKG_VERSION"));
    }

    #[tokio::test]
    async fn b1_routes_return_contract_conformant_responses() {
        let state = test_state();
        let app = router(state.clone());
        let contract = loom_contract::Contract::load();

        assert_b1_get(
            &contract,
            &app,
            "projects.sidebarBootstrap",
            "/api/v1/sidebar-bootstrap",
        )
        .await;
        assert_b1_get(&contract, &app, "system.config", "/api/v1/system/config").await;
        assert_b1_get(
            &contract,
            &app,
            "system.environmentProviders",
            "/api/v1/system/environment-providers",
        )
        .await;
        assert_b1_get(
            &contract,
            &app,
            "system.executionOptions",
            "/api/v1/system/execution-options",
        )
        .await;
        assert_b1_get(
            &contract,
            &app,
            "system.providers",
            "/api/v1/system/providers",
        )
        .await;
        assert_b1_get(
            &contract,
            &app,
            "system.providerStates",
            "/api/v1/system/providers/state",
        )
        .await;
        assert_b1_get(&contract, &app, "system.version", "/api/v1/system/version").await;

        let created = body_json(
            post(
                &app,
                "/api/v1/threads",
                serde_json::json!({
                    "projectId": state.registry.personal_project_id().to_string(),
                    "origin": "app",
                    "input": [],
                    "environment": { "type": "project-default" }
                }),
            )
            .await,
        )
        .await;
        let thread_id = created["id"]
            .as_str()
            .expect("created thread id")
            .to_owned();

        assert_b1_get(
            &contract,
            &app,
            "threads.events",
            &format!("/api/v1/threads/{thread_id}/events"),
        )
        .await;
        assert_b1_get(
            &contract,
            &app,
            "threads.get",
            &format!("/api/v1/threads/{thread_id}"),
        )
        .await;
        assert_b1_get(
            &contract,
            &app,
            "threads.output",
            &format!("/api/v1/threads/{thread_id}/output"),
        )
        .await;

        let read_path = format!("/api/v1/threads/{thread_id}/read");
        let response = post(&app, &read_path, serde_json::json!({})).await;
        assert_eq!(response.status(), StatusCode::OK, "POST {read_path}");
        let body = body_json(response).await;
        assert_b1_response(&contract, "threads.read", "POST", &body);

        let send_path = format!("/api/v1/threads/{thread_id}/send");
        let response = post(
            &app,
            &send_path,
            serde_json::json!({
                "input": [{ "type": "text", "text": "hello" }],
                "mode": "start"
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK, "POST {send_path}");
        let body = body_json(response).await;
        assert_b1_response(&contract, "threads.send", "POST", &body);

        let events_path = format!("/api/v1/threads/{thread_id}/events");
        let events_response = get(&app, &events_path).await;
        assert_eq!(
            events_response.status(),
            StatusCode::OK,
            "GET {events_path}"
        );
        let events = body_json(events_response).await;
        assert!(
            !events.as_array().expect("thread events array").is_empty(),
            "a dispatched send should leave a contract ThreadEvent"
        );
        let row = &events[0];
        assert!(row["id"].is_string());
        assert!(row["scope"].is_object());
        assert_eq!(row["threadId"], thread_id);
        assert!(row["seq"].is_number());
        assert!(row["createdAt"].is_number());
        assert!(row["type"].is_string());
        assert!(row["data"].is_object());
        assert!(row.get("event").is_none());

        let mut inner = row["data"].clone();
        let inner_object = inner.as_object_mut().expect("event data object");
        inner_object.insert("threadId".into(), row["threadId"].clone());
        inner_object.insert("scope".into(), row["scope"].clone());
        inner_object.insert("type".into(), row["type"].clone());
        let violations = contract.validate_thread_event(&inner);
        assert!(
            violations.is_empty(),
            "projected ThreadEvent row data is invalid: {violations:?}\n{inner}"
        );

        assert_b1_get(
            &contract,
            &app,
            "threads.tabs",
            &format!("/api/v1/threads/{thread_id}/tabs"),
        )
        .await;
        assert_b1_get(
            &contract,
            &app,
            "threads.timeline",
            &format!("/api/v1/threads/{thread_id}/timeline"),
        )
        .await;

        let malformed = get(&app, "/api/v1/threads/not-a-thread").await;
        assert_eq!(malformed.status(), StatusCode::BAD_REQUEST);
        let malformed_body = body_json(malformed).await;
        assert!(contract.validate_error_body(&malformed_body).is_empty());
        assert!(malformed_body["code"].is_string());
        assert!(malformed_body["message"].is_string());

        state.shutdown();
    }

    /// The request half of conformance, the gap W-554 found: B1's responses
    /// conformed while its request bodies did not, and no test could see it.
    ///
    /// Every write route that carries a JSON body is exercised twice — once
    /// with the shape the contract declares, which must be accepted, and once
    /// with a body that violates it, which must be a `422` in the uniform
    /// error shape. `validate_contract_request` is what makes the second half
    /// true at runtime.
    #[tokio::test]
    async fn write_routes_accept_and_reject_by_contract() {
        let state = test_state();
        let app = router(state.clone());
        let contract = loom_contract::Contract::load();
        let host = state
            .registry
            .enroll_host(None, "laptop".into(), loom_relay::now_ms())
            .unwrap()
            .0;
        let project_id = state.registry.personal_project_id().to_string();

        // Valid contract shape passes and is validated against the contract.
        let create = serde_json::json!({
            "projectId": project_id,
            "origin": "app",
            "input": [{ "type": "text", "text": "hello" }],
            "environment": { "type": "project-default" }
        });
        assert!(
            contract
                .validate_request_by_id("threads.create", &create)
                .is_empty(),
            "the test body itself must be contract-shaped"
        );
        let response = post(&app, "/api/v1/threads", create).await;
        assert_eq!(response.status(), StatusCode::CREATED);
        let created = body_json(response).await;
        assert_b1_response_status(&contract, "threads.create", "POST", 201, &created);
        let thread_id = created["id"].as_str().unwrap().to_string();

        // The pre-contract snake_case body the issue found being accepted is
        // now rejected before the handler runs.
        let response = post(
            &app,
            "/api/v1/threads",
            serde_json::json!({ "project_id": project_id }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let body = body_json(response).await;
        assert!(contract.validate_error_body(&body).is_empty());
        assert_eq!(body["code"], "invalid_request");
        assert!(
            body["message"].as_str().unwrap().contains("projectId"),
            "the rejection names the missing field: {body}"
        );

        // `threads.send`: missing `mode`, then a mode outside the enum.
        let send_path = format!("/api/v1/threads/{thread_id}/send");
        let response = post(
            &app,
            &send_path,
            serde_json::json!({ "input": [{ "type": "text", "text": "hi" }] }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let response = post(
            &app,
            &send_path,
            serde_json::json!({
                "input": [{ "type": "text", "text": "hi" }],
                "mode": "not-a-mode"
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);

        // A valid send still works after the rejections.
        let response = post(
            &app,
            &send_path,
            serde_json::json!({
                "input": [{ "type": "text", "text": "hello" }],
                "mode": "start"
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);

        // `projects.create`: a missing `source` is rejected; the contract
        // shape creates the project and its first source.
        let response = post(
            &app,
            "/api/v1/projects",
            serde_json::json!({ "name": "loom" }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);

        let response = post(
            &app,
            "/api/v1/projects",
            serde_json::json!({
                "name": "loom",
                "source": { "hostId": host.id.to_string(), "type": "local_path", "path": "/srv/loom" }
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::CREATED);
        let project = body_json(response).await;
        assert_b1_response_status(&contract, "projects.create", "POST", 201, &project);
        let new_project_id = project["id"].as_str().unwrap().to_string();

        // `projects.createSource`: `hostId` and `type` are required.
        let response = post(
            &app,
            &format!("/api/v1/projects/{new_project_id}/sources"),
            serde_json::json!({ "path": "/srv/other" }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);

        let response = post(
            &app,
            &format!("/api/v1/projects/{new_project_id}/sources"),
            serde_json::json!({
                "hostId": host.id.to_string(),
                "type": "local_path",
                "path": "/srv/other"
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::CREATED);
        let source = body_json(response).await;
        assert_b1_response_status(&contract, "projects.createSource", "POST", 201, &source);

        // `projects.update` is a `PATCH`; an unknown field is rejected and an
        // empty body is rejected by the handler's own "at least one field".
        let response = patch(
            &app,
            &format!("/api/v1/projects/{new_project_id}"),
            serde_json::json!({ "name": "loom-2" }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let renamed = body_json(response).await;
        assert_b1_response(&contract, "projects.update", "PATCH", &renamed);

        state.shutdown();
    }

    /// A contract-external loom route must not be caught by the request
    /// middleware; it keeps its own handler-level validation.
    #[tokio::test]
    async fn contract_external_routes_are_not_request_validated() {
        let state = test_state();
        let app = router(state.clone());
        // `/messages` is loom's reference-UI compatibility endpoint. Its body
        // is snake_case and is not in the bb contract, so a handler status —
        // not a 422 from the middleware — is the proof it was not swallowed
        // by contract validation.
        let response = post(
            &app,
            "/api/v1/threads/not-a-thread/messages",
            serde_json::json!({ "content": "hi" }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        state.shutdown();
    }

    #[tokio::test]
    async fn publish_stores_an_event_and_returns_its_id() {
        let state = test_state();
        let app = router(state.clone());
        let body = serde_json::json!({
            "scope": { "kind": "thread", "id": "thr_1" },
            "payload": "{\"n\":1}"
        });

        let response = app
            .oneshot(
                Request::post("/api/v1/publish")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let json = body_json(response).await;
        assert_eq!(json["scope"]["kind"], "thread");
        assert_eq!(json["event_id"].as_str().unwrap().len(), 26);

        let replayed = state
            .relay
            .replay_scope(&Scope::Thread("thr_1".into()), 10)
            .unwrap();
        assert_eq!(replayed.len(), 1);
    }

    #[tokio::test]
    async fn a_malformed_publish_body_is_rejected() {
        let app = router(test_state());
        let response = app
            .oneshot(
                Request::post("/api/v1/publish")
                    .header("content-type", "application/json")
                    .body(Body::from("{\"scope\":{\"kind\":\"nope\"}}"))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let body = body_json(response).await;
        let contract = loom_contract::Contract::load();
        assert!(contract.validate_error_body(&body).is_empty());
        assert_eq!(body["code"], "invalid_request");
        assert!(body["message"].is_string());
    }

    #[tokio::test]
    async fn extractor_failures_use_the_uniform_api_error_body() {
        let contract = loom_contract::Contract::load();
        let state = test_state();
        let app = router(state.clone());

        let malformed_json = app
            .clone()
            .oneshot(
                Request::post("/api/v1/publish")
                    .header("content-type", "application/json")
                    .body(Body::from("{\"scope\":"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(malformed_json.status(), StatusCode::BAD_REQUEST);
        let malformed_json_body = body_json(malformed_json).await;
        assert!(contract
            .validate_error_body(&malformed_json_body)
            .is_empty());
        assert_eq!(malformed_json_body["code"], "invalid_request");

        let created = body_json(
            post(
                &app,
                "/api/v1/threads",
                serde_json::json!({
                    "projectId": state.registry.personal_project_id().to_string(),
                    "origin": "app",
                    "input": [],
                    "environment": { "type": "project-default" }
                }),
            )
            .await,
        )
        .await;
        let thread_id = created["id"].as_str().unwrap();
        let malformed_query = get(
            &app,
            &format!("/api/v1/threads/{thread_id}/events?limit=%ZZ"),
        )
        .await;
        assert_eq!(malformed_query.status(), StatusCode::BAD_REQUEST);
        let malformed_query_body = body_json(malformed_query).await;
        assert!(contract
            .validate_error_body(&malformed_query_body)
            .is_empty());
        assert_eq!(malformed_query_body["code"], "invalid_request");

        state.shutdown();
    }

    #[tokio::test]
    async fn creating_a_thread_requires_a_project_and_the_seeded_one_works() {
        let state = test_state();
        let app = router(state.clone());
        let project_id = state.registry.personal_project_id().to_string();

        // No project is a schema rejection, not a silent landing in the seeded
        // one. The contract requires `projectId`/`origin`/`input`/`environment`,
        // so the body never reaches the handler.
        let missing = post(&app, "/api/v1/threads", serde_json::json!({})).await;
        assert_eq!(missing.status(), StatusCode::UNPROCESSABLE_ENTITY);

        let response = post(
            &app,
            "/api/v1/threads",
            serde_json::json!({
                "projectId": project_id,
                "origin": "app",
                "input": [],
                "environment": { "type": "project-default" }
            }),
        )
        .await;
        // The contract declares 201 for a created thread.
        assert_eq!(response.status(), StatusCode::CREATED);
        let json = body_json(response).await;
        assert_eq!(json["status"], "idle");
        assert_eq!(
            json["projectId"],
            state.registry.personal_project_id().to_string()
        );
        assert!(json["id"].as_str().unwrap().starts_with("thr_"));

        // `thread_created` lands in the project scope, not the thread scope:
        // it is the project's thread list that has to learn about it.
        let project = Scope::Project(state.registry.personal_project_id().to_string());
        let events = stored_events(&state, &project);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["type"], "thread_created");

        state.shutdown();
    }

    #[tokio::test]
    async fn creating_a_thread_in_an_unknown_project_is_not_found() {
        let app = router(test_state());
        let response = post(
            &app,
            "/api/v1/threads",
            serde_json::json!({
                "projectId": loom_domain::ProjectId::mint().to_string(),
                "origin": "app",
                "input": [],
                "environment": { "type": "project-default" }
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_project_is_created_listed_renamed_and_archived() {
        let state = test_state();
        let app = router(state.clone());
        // A source names a host, so one has to exist before the project can.
        let host = state
            .registry
            .enroll_host(None, "laptop".into(), loom_relay::now_ms())
            .unwrap()
            .0;

        let response = post(
            &app,
            "/api/v1/projects",
            serde_json::json!({
                "name": "loom",
                "source": { "hostId": host.id.to_string(), "type": "local_path", "path": "/srv/loom" }
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::CREATED);
        let created = body_json(response).await;
        let project_id = created["id"].as_str().unwrap().to_string();
        assert_eq!(created["name"], "loom");
        // The contract pairs creation with its source, so there is one here.
        assert_eq!(created["sources"][0]["path"], "/srv/loom");

        // `project_created` is published to `global`: the project list is not
        // scoped to a project a client can already have subscribed to.
        let events = stored_events(&state, &Scope::Global);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["type"], "project_created");
        assert_eq!(events[0]["project"]["id"], project_id);

        // The seeded project and the new one are both listed, seeded first.
        let listed = body_json(get(&app, "/api/v1/projects").await).await;
        let ids: Vec<&str> = listed
            .as_array()
            .unwrap()
            .iter()
            .map(|project| project["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids.len(), 2);
        assert_eq!(ids[0], state.registry.personal_project_id().to_string());
        assert!(ids.contains(&project_id.as_str()));

        // A rename publishes to the project's own scope.
        let renamed = body_json(
            patch(
                &app,
                &format!("/api/v1/projects/{project_id}"),
                serde_json::json!({ "name": "loom-2" }),
            )
            .await,
        )
        .await;
        assert_eq!(renamed["name"], "loom-2");
        let project = Scope::Project(project_id.clone());
        let events = stored_events(&state, &project);
        assert_eq!(events.last().unwrap()["type"], "project_updated");
        assert_eq!(events.last().unwrap()["project"]["name"], "loom-2");

        // Archive is idempotent-rejecting, not idempotent.
        let archived = post(
            &app,
            &format!("/api/v1/projects/{project_id}/archive"),
            serde_json::json!({}),
        )
        .await;
        assert_eq!(archived.status(), StatusCode::OK);
        assert!(body_json(archived).await["archived_at_ms"].is_number());
        let again = post(
            &app,
            &format!("/api/v1/projects/{project_id}/archive"),
            serde_json::json!({}),
        )
        .await;
        assert_eq!(again.status(), StatusCode::CONFLICT);

        state.shutdown();
    }

    #[tokio::test]
    async fn project_sources_are_added_and_removed() {
        let state = test_state();
        let app = router(state.clone());
        let host = state
            .registry
            .enroll_host(None, "laptop".into(), loom_relay::now_ms())
            .unwrap()
            .0;
        let created = body_json(
            post(
                &app,
                "/api/v1/projects",
                serde_json::json!({
                "name": "loom",
                "source": { "hostId": host.id.to_string(), "type": "local_path", "path": "/srv/loom" }
            }),
            )
            .await,
        )
        .await;
        let project_id = created["id"].as_str().unwrap().to_string();

        let response = post(
            &app,
            &format!("/api/v1/projects/{project_id}/sources"),
            serde_json::json!({
                "hostId": host.id.to_string(),
                "type": "local_path",
                "path": "/srv/loom-2",
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::CREATED);
        // `projects.createSource` returns the created source, not the project.
        let source = body_json(response).await;
        assert_eq!(source["path"], "/srv/loom-2");
        assert_eq!(source["hostId"], host.id.to_string());
        assert_eq!(source["isDefault"], false);
        assert_eq!(source["type"], "local_path");
        let source_id = source["id"].as_str().unwrap().to_string();

        // The update is in the project's scope, ready for a client replay.
        let scope = Scope::Project(project_id.clone());
        let events = stored_events(&state, &scope);
        assert_eq!(events.last().unwrap()["type"], "project_updated");

        let removed = delete(
            &app,
            &format!("/api/v1/projects/{project_id}/sources/{source_id}"),
        )
        .await;
        assert_eq!(removed.status(), StatusCode::OK);
        assert_eq!(body_json(removed).await["ok"], true);

        // Removing it again is a not-found, not a silent success.
        let again = delete(
            &app,
            &format!("/api/v1/projects/{project_id}/sources/{source_id}"),
        )
        .await;
        assert_eq!(again.status(), StatusCode::NOT_FOUND);

        state.shutdown();
    }

    #[tokio::test]
    async fn archiving_a_project_with_a_run_in_flight_is_a_conflict() {
        let state = test_state();
        let app = router(state.clone());
        let project_id = state.registry.personal_project_id();
        // A connected host and a bound environment, so the message actually
        // dispatches a run and leaves the thread `working`.
        let host = state
            .registry
            .enroll_host(None, "laptop".into(), loom_relay::now_ms())
            .unwrap()
            .0;
        let environment = state
            .registry
            .create_environment(
                Some(project_id.clone()),
                host.id,
                EnvironmentKind::Unmanaged,
                Some("/srv/loom".into()),
                loom_relay::now_ms(),
            )
            .unwrap()
            .0;
        let created = body_json(
            post(
                &app,
                "/api/v1/threads",
                serde_json::json!({
                    "projectId": project_id.to_string(),
                    "origin": "app",
                    "input": [],
                    "environment": { "type": "reuse", "environmentId": environment.id.to_string() }
                }),
            )
            .await,
        )
        .await;
        let thread_id = created["id"].as_str().unwrap().to_string();
        // A user message moves the thread to `working`.
        post(
            &app,
            &format!("/api/v1/threads/{thread_id}/messages"),
            serde_json::json!({ "content": "hi" }),
        )
        .await;
        assert_eq!(
            state
                .registry
                .thread(&thread_id.parse().unwrap())
                .unwrap()
                .status,
            loom_domain::ThreadStatus::Working
        );

        let response = post(
            &app,
            &format!("/api/v1/projects/{project_id}/archive"),
            serde_json::json!({}),
        )
        .await;
        assert_eq!(response.status(), StatusCode::CONFLICT);

        // Refuse, never cascade: the thread keeps its project.
        assert_eq!(
            state
                .registry
                .thread(&thread_id.parse().unwrap())
                .unwrap()
                .project_id,
            project_id
        );
        state.shutdown();
    }

    #[tokio::test]
    async fn a_user_message_publishes_message_then_status_to_the_thread_scope() {
        let state = test_state();
        // A connected host and a bound environment so the message actually
        // dispatches a run; without either the dispatcher would fail the thread
        // on the spot and append terminal events this test does not want.
        let (host, _) = state
            .registry
            .enroll_host(None, "laptop".into(), loom_relay::now_ms())
            .unwrap();
        let (environment, _) = state
            .registry
            .create_environment(
                Some(state.registry.personal_project_id()),
                host.id,
                EnvironmentKind::Unmanaged,
                Some("/srv/loom".into()),
                loom_relay::now_ms(),
            )
            .unwrap();
        let app = router(state.clone());
        let created = body_json(
            post(
                &app,
                "/api/v1/threads",
                serde_json::json!({
                    "projectId": state.registry.personal_project_id().to_string(),
                    "origin": "app",
                    "input": [],
                    "environment": { "type": "reuse", "environmentId": environment.id.to_string() }
                }),
            )
            .await,
        )
        .await;
        let thread_id = created["id"].as_str().unwrap().to_string();

        let response = post(
            &app,
            &format!("/api/v1/threads/{thread_id}/messages"),
            serde_json::json!({ "content": "hello" }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let json = body_json(response).await;
        let published = json["events"].as_array().unwrap();
        assert_eq!(published.len(), 2);
        assert_eq!(published[0]["event_type"], "thread_message_added");
        assert_eq!(published[0]["scope"]["kind"], "thread");
        assert_eq!(published[1]["event_type"], "thread_status_changed");
        assert_eq!(published[1]["scope"]["id"], thread_id);

        let scope = Scope::Thread(thread_id);
        let stored = stored_events(&state, &scope);
        assert_eq!(stored.len(), 2);
        assert_eq!(stored[0]["type"], "thread_message_added");
        assert_eq!(stored[1]["type"], "thread_status_changed");

        state.shutdown();
    }

    #[tokio::test]
    async fn messaging_an_unknown_thread_is_not_found() {
        let app = router(test_state());
        let response = post(
            &app,
            &format!("/api/v1/threads/{}/messages", ThreadId::mint()),
            serde_json::json!({ "content": "hello" }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn an_empty_message_is_rejected() {
        let state = test_state();
        let app = router(state.clone());
        let created = body_json(
            post(
                &app,
                "/api/v1/threads",
                serde_json::json!({
                    "projectId": state.registry.personal_project_id().to_string(),
                    "origin": "app",
                    "input": [],
                    "environment": { "type": "project-default" }
                }),
            )
            .await,
        )
        .await;
        let thread_id = created["id"].as_str().unwrap().to_string();

        let response = post(
            &app,
            &format!("/api/v1/threads/{thread_id}/messages"),
            serde_json::json!({ "content": "   " }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn a_malformed_thread_id_is_rejected() {
        let app = router(test_state());
        let response = post(
            &app,
            "/api/v1/threads/not-a-thread/messages",
            serde_json::json!({ "content": "hello" }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn registering_a_host_publishes_to_the_host_scope() {
        let state = test_state();
        let app = router(state.clone());

        let response = post(
            &app,
            "/api/v1/hosts",
            serde_json::json!({ "name": "laptop" }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let json = body_json(response).await;
        assert_eq!(json["host"]["status"], "connected");
        let host_id = json["host"]["id"].as_str().unwrap().to_string();

        let scope = Scope::Host(host_id);
        let events = stored_events(&state, &scope);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["type"], "host_registered");

        state.shutdown();
    }

    #[tokio::test]
    async fn a_server_with_no_daemon_stays_up_and_reports_no_primary_host() {
        let state = test_state();
        let app = router(state.clone());

        // Health must answer whether or not any daemon ever connected.
        let health = get(&app, "/health").await;
        assert_eq!(health.status(), StatusCode::OK);
        assert_eq!(body_json(health).await["status"], "ok");

        // The primary-host lookup degrades to an explicit "no host", never an
        // error, so file browsing is not stranded on an absent local daemon.
        let primary = get(&app, "/api/v1/hosts/primary").await;
        assert_eq!(primary.status(), StatusCode::OK);
        let json = body_json(primary).await;
        assert!(json["host"].is_null());
        assert_eq!(json["source"], "no_host");

        let hosts = body_json(get(&app, "/api/v1/hosts").await).await;
        assert_eq!(hosts.as_array().unwrap().len(), 0);

        state.shutdown();
    }

    #[tokio::test]
    async fn a_remote_host_becomes_primary_and_reconnects_keep_its_identity() {
        // The server declares a local host that never enrolls — exactly the
        // server-only case.
        let local = HostId::mint();
        let state = AppState::build(AppConfig {
            local_host_id: Some(local.clone()),
            ..AppConfig::default()
        })
        .unwrap();
        let app = router(state.clone());

        let created = body_json(
            post(
                &app,
                "/api/v1/hosts",
                serde_json::json!({ "name": "remote-1" }),
            )
            .await,
        )
        .await;
        let host_id = created["host"]["id"].as_str().unwrap().to_string();

        // The absent local host does not win; the remote one is primary.
        let primary = body_json(get(&app, "/api/v1/hosts/primary").await).await;
        assert_eq!(primary["source"], "remote");
        assert_eq!(primary["host"]["id"], host_id);

        // Re-enrolling with the same id is idempotent.
        let again = body_json(
            post(
                &app,
                "/api/v1/hosts",
                serde_json::json!({ "id": host_id, "name": "remote-1" }),
            )
            .await,
        )
        .await;
        assert_eq!(again["host"]["id"], host_id);
        assert_eq!(again["event_id"], "", "a no-op reconnect publishes nothing");

        let hosts = body_json(get(&app, "/api/v1/hosts").await).await;
        assert_eq!(hosts.as_array().unwrap().len(), 1);

        state.shutdown();
    }

    #[tokio::test]
    async fn heartbeat_and_disconnect_move_the_host_status() {
        let state = test_state();
        let app = router(state.clone());
        let created = body_json(
            post(
                &app,
                "/api/v1/hosts",
                serde_json::json!({ "name": "laptop" }),
            )
            .await,
        )
        .await;
        let host_id = created["host"]["id"].as_str().unwrap().to_string();

        let beat = post(
            &app,
            &format!("/api/v1/hosts/{host_id}/heartbeat"),
            serde_json::json!({}),
        )
        .await;
        assert_eq!(beat.status(), StatusCode::OK);

        let gone = post(
            &app,
            &format!("/api/v1/hosts/{host_id}/disconnect"),
            serde_json::json!({}),
        )
        .await;
        assert_eq!(gone.status(), StatusCode::OK);

        let primary = body_json(get(&app, "/api/v1/hosts/primary").await).await;
        assert_eq!(primary["source"], "no_host");

        let hosts = body_json(get(&app, "/api/v1/hosts").await).await;
        assert_eq!(hosts[0]["status"], "disconnected");

        state.shutdown();
    }

    #[tokio::test]
    async fn a_declared_local_host_wins_when_it_is_connected() {
        let state = test_state();
        let local = HostId::mint();
        state
            .registry
            .enroll_host(
                Some(local.clone()),
                "this-machine".into(),
                loom_relay::now_ms(),
            )
            .unwrap();
        let remote = state
            .registry
            .enroll_host(None, "remote".into(), loom_relay::now_ms())
            .unwrap()
            .0;

        let primary = state.registry.primary_host(Some(&local)).unwrap();
        assert_eq!(primary.id, local);
        assert_ne!(primary.id, remote.id);
    }

    #[tokio::test]
    async fn the_thread_list_is_the_uis_entry_point() {
        let state = test_state();
        let app = router(state.clone());

        // Empty to begin with, and an empty list is not an error.
        let empty = body_json(get(&app, "/api/v1/threads").await).await;
        assert_eq!(empty.as_array().unwrap().len(), 0);

        let owned = serde_json::json!({
            "projectId": state.registry.personal_project_id().to_string(),
            "origin": "app",
            "input": [],
            "environment": { "type": "project-default" }
        });
        let first = body_json(post(&app, "/api/v1/threads", owned.clone()).await).await;
        let second = body_json(
            post(
                &app,
                "/api/v1/threads",
                serde_json::json!({
                    "projectId": state.registry.personal_project_id().to_string(),
                    "origin": "app",
                    "input": [],
                    "environment": { "type": "project-default" },
                    "title": "second",
                }),
            )
            .await,
        )
        .await;

        let listed = body_json(get(&app, "/api/v1/threads").await).await;
        let ids: Vec<&str> = listed
            .as_array()
            .unwrap()
            .iter()
            .map(|thread| thread["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&first["id"].as_str().unwrap()));
        assert!(ids.contains(&second["id"].as_str().unwrap()));

        state.shutdown();
    }

    #[tokio::test]
    async fn creating_an_unmanaged_environment_returns_it_ready() {
        let state = test_state();
        state
            .registry
            .enroll_host(None, "laptop".into(), loom_relay::now_ms())
            .unwrap();
        let app = router(state.clone());

        let response = post(
            &app,
            "/api/v1/environments",
            serde_json::json!({
                "kind": "unmanaged",
                "path": "/srv/loom",
                "project_id": state.registry.personal_project_id().to_string(),
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let json = body_json(response).await;
        assert_eq!(json["environment"]["status"], "ready");
        assert_eq!(json["environment"]["path"], "/srv/loom");

        // The creation event is in the project scope, where a client replays
        // it alongside the thread list.
        let project = Scope::Project(
            json["environment"]["project_id"]
                .as_str()
                .unwrap()
                .to_owned(),
        );
        let events = stored_events(&state, &project);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["type"], "environment_created");
        state.shutdown();
    }

    #[tokio::test]
    async fn creating_a_managed_environment_dispatches_provisioning() {
        let state = test_state();
        let (host, _) = state
            .registry
            .enroll_host(None, "laptop".into(), loom_relay::now_ms())
            .unwrap();
        let app = router(state.clone());

        let response = post(
            &app,
            "/api/v1/environments",
            serde_json::json!({
                "kind": "managed",
                "project_id": state.registry.personal_project_id().to_string(),
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let json = body_json(response).await;
        // The creation snapshot is `creating`; the follow-up status change is
        // announced on the project scope.
        assert_eq!(json["environment"]["status"], "creating");
        assert!(json["environment"]["path"].is_null());

        // The environment did move on, though.
        assert_eq!(
            state
                .registry
                .environment(&json["environment"]["id"].as_str().unwrap().parse().unwrap())
                .unwrap()
                .status,
            EnvironmentStatus::Provisioning
        );

        // The request is in the host room, ready for a reconnecting daemon.
        let events = stored_events(&state, &Scope::Host(host.id.to_string()));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["environment_id"], json["environment"]["id"]);
        state.shutdown();
    }

    #[tokio::test]
    async fn an_unmanaged_environment_requires_an_absolute_path() {
        let state = test_state();
        state
            .registry
            .enroll_host(None, "laptop".into(), loom_relay::now_ms())
            .unwrap();
        let app = router(state.clone());

        let missing = post(
            &app,
            "/api/v1/environments",
            serde_json::json!({
                "kind": "unmanaged",
                "project_id": state.registry.personal_project_id().to_string(),
            }),
        )
        .await;
        assert_eq!(missing.status(), StatusCode::BAD_REQUEST);

        let relative = post(
            &app,
            "/api/v1/environments",
            serde_json::json!({
                "kind": "unmanaged",
                "path": "srv/loom",
                "project_id": state.registry.personal_project_id().to_string(),
            }),
        )
        .await;
        assert_eq!(relative.status(), StatusCode::BAD_REQUEST);
        state.shutdown();
    }

    #[tokio::test]
    async fn creating_an_environment_with_no_host_is_a_conflict() {
        let state = test_state();
        let app = router(state.clone());
        let response = post(
            &app,
            "/api/v1/environments",
            serde_json::json!({
                "kind": "unmanaged",
                "path": "/srv/loom",
                "project_id": state.registry.personal_project_id().to_string(),
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::CONFLICT);
        state.shutdown();
    }

    #[tokio::test]
    async fn environments_are_listable_by_project_and_fetchable_by_id() {
        let state = test_state();
        let (host, _) = state
            .registry
            .enroll_host(None, "laptop".into(), loom_relay::now_ms())
            .unwrap();
        let (environment, _) = state
            .registry
            .create_environment(
                Some(state.registry.personal_project_id()),
                host.id,
                EnvironmentKind::Unmanaged,
                Some("/srv/loom".into()),
                loom_relay::now_ms(),
            )
            .unwrap();
        let app = router(state.clone());

        let project_id = state.registry.personal_project_id();
        let listed = body_json(
            get(
                &app,
                &format!("/api/v1/environments?project_id={project_id}"),
            )
            .await,
        )
        .await;
        assert_eq!(listed.as_array().unwrap().len(), 1);

        let by_id = get(&app, &format!("/api/v1/environments/{}", environment.id)).await;
        assert_eq!(by_id.status(), StatusCode::OK);
        assert_eq!(body_json(by_id).await["id"], environment.id.to_string());

        let missing = get(
            &app,
            &format!("/api/v1/environments/{}", EnvironmentId::mint()),
        )
        .await;
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
        state.shutdown();
    }

    #[tokio::test]
    async fn destroying_an_environment_is_terminal() {
        let state = test_state();
        let (host, _) = state
            .registry
            .enroll_host(None, "laptop".into(), loom_relay::now_ms())
            .unwrap();
        let (environment, _) = state
            .registry
            .create_environment(
                Some(state.registry.personal_project_id()),
                host.id,
                EnvironmentKind::Unmanaged,
                Some("/srv/loom".into()),
                loom_relay::now_ms(),
            )
            .unwrap();
        let app = router(state.clone());

        let response = post(
            &app,
            &format!("/api/v1/environments/{}/destroy", environment.id),
            serde_json::json!({}),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_json(response).await["status"], "destroyed");

        // Destroyed is terminal, so a second destroy is a conflict.
        let again = post(
            &app,
            &format!("/api/v1/environments/{}/destroy", environment.id),
            serde_json::json!({}),
        )
        .await;
        assert_eq!(again.status(), StatusCode::CONFLICT);
        state.shutdown();
    }

    #[tokio::test]
    async fn the_root_path_serves_the_ui_shell() {
        let app = router(test_state());
        let response = get(&app, "/").await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert!(String::from_utf8_lossy(&body).contains("<title>loom</title>"));
    }

    /// A state whose artifact directory holds one real file.
    fn state_with_artifacts(dir: &std::path::Path) -> AppState {
        AppState::build(AppConfig {
            artifact_dir: Some(dir.to_path_buf()),
            ..AppConfig::default()
        })
        .unwrap()
    }

    #[tokio::test]
    async fn the_install_version_route_reports_the_protocol_the_daemon_compares() {
        let app = router(test_state());
        let response = get(&app, "/install/version").await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_json(response).await;
        assert_eq!(body["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
    }

    #[tokio::test]
    async fn the_artifact_route_serves_the_binary_with_its_digest_and_etag() {
        let dir = tempfile::tempdir().unwrap();
        let bytes = b"a stand-in for the daemon binary";
        std::fs::write(dir.path().join("loom-daemon"), bytes).unwrap();
        let app = router(state_with_artifacts(dir.path()));

        let response = get(&app, "/install/loom-daemon").await;
        assert_eq!(response.status(), StatusCode::OK);
        let digest = crate::artifacts::sha256_hex(bytes);
        assert_eq!(
            response.headers().get(artifacts::DIGEST_HEADER).unwrap(),
            digest.as_str()
        );
        assert_eq!(
            response.headers().get(axum::http::header::ETAG).unwrap(),
            format!("\"sha256-{digest}\"").as_str()
        );
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(body.as_ref(), bytes);
    }

    #[tokio::test]
    async fn a_conditional_artifact_request_for_the_same_digest_is_a_304() {
        let dir = tempfile::tempdir().unwrap();
        let bytes = b"an unchanged daemon binary";
        std::fs::write(dir.path().join("loom-daemon"), bytes).unwrap();
        let app = router(state_with_artifacts(dir.path()));
        let digest = crate::artifacts::sha256_hex(bytes);

        let response = app
            .clone()
            .oneshot(
                Request::get("/install/loom-daemon")
                    .header("if-none-match", format!("\"sha256-{digest}\""))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
        // The digest is still reported, so the client can persist what it
        // proved it already has.
        assert_eq!(
            response.headers().get(artifacts::DIGEST_HEADER).unwrap(),
            digest.as_str()
        );
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert!(body.is_empty(), "a 304 carries no body");

        // A different validator is not the same artifact, so the bytes come.
        let response = app
            .oneshot(
                Request::get("/install/loom-daemon")
                    .header("if-none-match", format!("\"sha256-{}\"", "0".repeat(64)))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn an_artifact_request_for_another_target_is_a_404_that_names_it() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("loom-daemon"), b"the local binary").unwrap();
        let app = router(state_with_artifacts(dir.path()));

        // The unnamed binary answers only for this server's own triple.
        let other = if crate::TARGET == "aarch64-unknown-linux-musl" {
            "x86_64-unknown-linux-musl"
        } else {
            "aarch64-unknown-linux-musl"
        };
        let response = get(&app, &format!("/install/loom-daemon?target={other}")).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body = body_json(response).await;
        assert_eq!(body["code"], "not_found");
        assert!(
            body["message"].as_str().unwrap().contains(other),
            "the message should name the target: {body}"
        );

        // A target that is not a triple at all is the client's mistake.
        let response = get(&app, "/install/loom-daemon?target=..%2F..%2Fetc%2Fpasswd").await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(body_json(response).await["code"], "invalid_request");
    }
}
