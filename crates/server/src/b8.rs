//! Batch B8: host management and host-owned filesystem reads.
//!
//! The control plane owns host identity and policy, but never opens a path on
//! a host's machine. Filesystem reads are sent through `HostFileBroker`; a
//! missing or disconnected daemon therefore returns a bounded transport error
//! instead of making the HTTP request wait forever.

#![allow(clippy::result_large_err)]

use std::path::Path as FsPath;

use axum::body::Body;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use loom_domain::{Host, HostId, HostPermissionMode};
use loom_provider_protocol::{HostFileOperation, HostFileOutcome};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::b5::{base64_decode, validate_absolute_path};
use crate::host_files::HostFileTransportError;
use crate::state::AppState;

fn api_error(status: StatusCode, code: &'static str, message: impl Into<String>) -> Response {
    (
        status,
        Json(json!({ "code": code, "message": message.into() })),
    )
        .into_response()
}

fn parse_host(raw: &str) -> Result<HostId, Response> {
    raw.parse::<HostId>().map_err(|error| {
        api_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            error.to_string(),
        )
    })
}

fn host_or_error(state: &AppState, raw: &str) -> Result<Host, Response> {
    let id = parse_host(raw)?;
    state.registry.host(&id).ok_or_else(|| {
        api_error(
            StatusCode::NOT_FOUND,
            "host_not_found",
            format!("host {id} is not known"),
        )
    })
}

fn host_value(host: &Host) -> Value {
    json!({
        "id": host.id.to_string(),
        "name": host.name,
        "type": "persistent",
        "status": host.status,
        "maxPermissionMode": host.max_permission_mode.as_str(),
        "lastSeenAt": host.last_seen_at_ms,
        "lastRejectedProtocolVersion": null,
        "createdAt": host.created_at_ms,
        "updatedAt": host.updated_at_ms,
    })
}

fn transport_error(error: HostFileTransportError) -> Response {
    match error {
        HostFileTransportError::Publish(message) => {
            api_error(StatusCode::BAD_GATEWAY, "host_unavailable", message)
        }
        HostFileTransportError::Timeout => api_error(
            StatusCode::GATEWAY_TIMEOUT,
            "command_timeout",
            "the host did not answer the filesystem request in time",
        ),
        HostFileTransportError::UnknownHost(message) => {
            api_error(StatusCode::NOT_FOUND, "host_not_found", message)
        }
    }
}

fn host_failure(code: &str, message: &str) -> Response {
    let (status, public_code) = match code {
        "not_found" | "path_not_found" => (StatusCode::NOT_FOUND, "not_found"),
        "invalid_path" | "invalid_request" => (StatusCode::BAD_REQUEST, "invalid_path"),
        "permission_denied" => (StatusCode::FORBIDDEN, "permission_denied"),
        _ => (StatusCode::BAD_GATEWAY, "host_unavailable"),
    };
    api_error(status, public_code, message)
}

fn listing_path(root: &str, relative: &str) -> String {
    if relative.is_empty() {
        return root.to_owned();
    }
    FsPath::new(root)
        .join(relative)
        .to_string_lossy()
        .into_owned()
}

fn file_response(content: loom_provider_protocol::HostFileContent) -> Response {
    let bytes = match content.content_encoding {
        loom_provider_protocol::HostFileEncoding::Utf8 => content.content.into_bytes(),
        loom_provider_protocol::HostFileEncoding::Base64 => match base64_decode(&content.content) {
            Some(bytes) => bytes,
            None => {
                return api_error(
                    StatusCode::BAD_GATEWAY,
                    "host_unavailable",
                    "host returned invalid file data",
                )
            }
        },
    };
    let mut response = Response::new(Body::from(bytes));
    if let Some(mime) = content.mime_type.and_then(|mime| mime.parse().ok()) {
        response.headers_mut().insert(header::CONTENT_TYPE, mime);
    }
    response
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DirectoryQuery {
    path: Option<String>,
}

pub async fn create_join_code() -> (StatusCode, Json<Value>) {
    let host_id = HostId::mint();
    let event_id = loom_relay::EventId::new().to_string();
    (
        StatusCode::CREATED,
        Json(json!({
            "joinCode": format!("loom-{}", event_id.chars().take(16).collect::<String>()),
            "hostId": host_id.to_string(),
            "expiresAt": loom_relay::now_ms().saturating_add(10 * 60 * 1_000),
        })),
    )
}

pub async fn host_get(State(state): State<AppState>, AxumPath(raw): AxumPath<String>) -> Response {
    match host_or_error(&state, &raw) {
        Ok(host) => Json(host_value(&host)).into_response(),
        Err(response) => response,
    }
}

#[derive(Debug, Deserialize)]
pub struct HostUpdateRequest {
    pub name: String,
}

pub async fn host_update(
    State(state): State<AppState>,
    AxumPath(raw): AxumPath<String>,
    Json(request): Json<HostUpdateRequest>,
) -> Response {
    let host_id = match parse_host(&raw) {
        Ok(id) => id,
        Err(response) => return response,
    };
    match state
        .registry
        .rename_host(&host_id, request.name, loom_relay::now_ms())
    {
        Ok(host) => Json(host_value(&host)).into_response(),
        Err(error) => crate::http::command_error_response(error),
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionCeilingRequest {
    max_permission_mode: HostPermissionMode,
}

pub async fn host_permission_ceiling(
    State(state): State<AppState>,
    AxumPath(raw): AxumPath<String>,
    Json(request): Json<PermissionCeilingRequest>,
) -> Response {
    let host_id = match parse_host(&raw) {
        Ok(id) => id,
        Err(response) => return response,
    };
    match state.registry.update_host_permission_ceiling(
        &host_id,
        request.max_permission_mode,
        loom_relay::now_ms(),
    ) {
        Ok(host) => Json(host_value(&host)).into_response(),
        Err(error) => crate::http::command_error_response(error),
    }
}

pub async fn host_delete(
    State(state): State<AppState>,
    AxumPath(raw): AxumPath<String>,
) -> Response {
    let host_id = match parse_host(&raw) {
        Ok(id) => id,
        Err(response) => return response,
    };
    match state.registry.delete_host(&host_id) {
        Ok(()) => Json(json!({ "ok": true })).into_response(),
        Err(error) => crate::http::command_error_response(error),
    }
}

pub async fn host_directory(
    State(state): State<AppState>,
    AxumPath(raw): AxumPath<String>,
    Query(query): Query<DirectoryQuery>,
) -> Response {
    let host = match host_or_error(&state, &raw) {
        Ok(host) => host,
        Err(response) => return response,
    };
    let directory = query.path.unwrap_or_else(|| "/".into());
    if validate_absolute_path(&directory).is_err() {
        return api_error(
            StatusCode::BAD_REQUEST,
            "invalid_path",
            "path must be absolute",
        );
    }
    let outcome = state
        .request_host_file(
            &host.id,
            HostFileOperation::List {
                path: directory.clone(),
                query: None,
                limit: 10_000,
                include_files: true,
                include_directories: true,
                include_hidden: true,
            },
        )
        .await;
    match outcome {
        Err(error) => transport_error(error),
        Ok(HostFileOutcome::Listing { entries, .. }) => {
            let entries = entries.into_iter().map(|entry| json!({
                "kind": match entry.kind { loom_provider_protocol::HostPathKind::File => "file", loom_provider_protocol::HostPathKind::Directory => "directory" },
                "name": entry.name,
                "path": listing_path(&directory, &entry.path),
            })).collect::<Vec<_>>();
            let parent = FsPath::new(&directory)
                .parent()
                .map(|path| path.to_string_lossy().into_owned());
            Json(json!({ "directory": directory, "parent": parent, "entries": entries }))
                .into_response()
        }
        Ok(HostFileOutcome::Failed { code, message }) => host_failure(&code, &message),
        Ok(_) => api_error(
            StatusCode::BAD_GATEWAY,
            "host_unavailable",
            "host returned an invalid directory response",
        ),
    }
}

#[derive(Debug, Deserialize)]
pub struct PathsExistRequest {
    pub paths: Vec<String>,
}

pub async fn host_paths_exist(
    State(state): State<AppState>,
    AxumPath(raw): AxumPath<String>,
    Json(request): Json<PathsExistRequest>,
) -> Response {
    let host = match host_or_error(&state, &raw) {
        Ok(host) => host,
        Err(response) => return response,
    };
    if request
        .paths
        .iter()
        .any(|path| validate_absolute_path(path).is_err())
    {
        return api_error(
            StatusCode::BAD_REQUEST,
            "invalid_path",
            "paths must be absolute",
        );
    }
    let requested = request.paths.clone();
    match state
        .request_host_file(
            &host.id,
            HostFileOperation::Exists {
                paths: requested.clone(),
            },
        )
        .await
    {
        Err(error) => transport_error(error),
        Ok(HostFileOutcome::Listing { entries, .. }) => {
            let existing = entries
                .into_iter()
                .map(|entry| entry.path)
                .collect::<std::collections::HashSet<_>>();
            let existence = requested
                .into_iter()
                .map(|path| {
                    let exists = existing.contains(&path);
                    (path, Value::Bool(exists))
                })
                .collect::<serde_json::Map<_, _>>();
            Json(json!({ "existence": existence })).into_response()
        }
        Ok(HostFileOutcome::Failed { code, message }) => host_failure(&code, &message),
        Ok(_) => api_error(
            StatusCode::BAD_GATEWAY,
            "host_unavailable",
            "host returned an invalid existence response",
        ),
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PickFolderRequest {
    client_host_id: String,
}

pub async fn host_pick_folder(
    State(state): State<AppState>,
    AxumPath(raw): AxumPath<String>,
    Json(request): Json<PickFolderRequest>,
) -> Response {
    let _ = match host_or_error(&state, &raw) {
        Ok(host) => host,
        Err(response) => return response,
    };
    // Folder dialogs are a client capability. The server has no desktop to
    // open one on, so server-only and remote deployments report cancellation.
    if request.client_host_id.trim().is_empty() {
        return api_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "clientHostId must not be empty",
        );
    }
    Json(json!({ "path": Value::Null })).into_response()
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CloneDefaultQuery {
    project_id: String,
}

pub async fn host_clone_default_path(
    State(state): State<AppState>,
    AxumPath(raw): AxumPath<String>,
    Query(query): Query<CloneDefaultQuery>,
) -> Response {
    let host = match host_or_error(&state, &raw) {
        Ok(host) => host,
        Err(response) => return response,
    };
    let project_id = match query.project_id.parse::<loom_domain::ProjectId>() {
        Ok(id) => id,
        Err(error) => {
            return api_error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                error.to_string(),
            )
        }
    };
    let Some(project) = state.registry.project(&project_id) else {
        return api_error(
            StatusCode::NOT_FOUND,
            "project_not_found",
            "project is not known",
        );
    };
    if let Some(source) = project
        .sources
        .iter()
        .find(|source| source.host_id == host.id && !source.path.is_empty())
    {
        return Json(json!({ "path": source.path })).into_response();
    }
    api_error(
        StatusCode::NOT_FOUND,
        "path_not_found",
        "the project has no checkout on this host",
    )
}

pub async fn provider_cli_status(
    State(state): State<AppState>,
    AxumPath(raw): AxumPath<String>,
) -> Response {
    let _host = match host_or_error(&state, &raw) {
        Ok(host) => host,
        Err(response) => return response,
    };
    let provider = state.provider_spec().name.clone();
    // Providers are first-class ACP capabilities. There is intentionally no
    // legacy CLI installer to report as installed or execute remotely.
    Json(json!({ provider.clone(): {
        "displayName": provider,
        "executableName": state.provider_spec().command,
        "executablePath": Value::Null,
        "installed": false,
        "installSource": "notInstalled",
        "currentVersion": Value::Null,
        "latestVersion": Value::Null,
        "minimumSupportedVersion": Value::Null,
        "npmPackageName": Value::Null,
        "npmGlobalPackageVersion": Value::Null,
        "installAction": Value::Null,
        "needsUpdate": false,
        "versionUnsupported": false,
    }}))
    .into_response()
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderCliInstallRequest {
    provider: String,
    action_kind: String,
}

pub async fn provider_cli_install(
    State(state): State<AppState>,
    AxumPath(raw): AxumPath<String>,
    Json(request): Json<ProviderCliInstallRequest>,
) -> Response {
    let _ = match host_or_error(&state, &raw) {
        Ok(host) => host,
        Err(response) => return response,
    };
    if request.provider != state.provider_spec().name
        || !matches!(request.action_kind.as_str(), "install" | "update")
    {
        return api_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "provider CLI is not configured",
        );
    }
    (
        StatusCode::NOT_IMPLEMENTED,
        "ACP providers do not use a provider CLI",
    )
        .into_response()
}

pub async fn host_retry_update(
    State(state): State<AppState>,
    AxumPath(raw): AxumPath<String>,
) -> Response {
    let _ = match host_or_error(&state, &raw) {
        Ok(host) => host,
        Err(response) => return response,
    };
    (StatusCode::NOT_IMPLEMENTED, Json(json!({ "code": "not_supported", "message": "daemon self-update is negotiated on reconnect" }))).into_response()
}

pub async fn system_attention(State(state): State<AppState>) -> Json<Value> {
    Json(json!({ "hasAttention": state.registry.has_pending_interactions() }))
}

pub async fn file_preview_content(
    State(state): State<AppState>,
    AxumPath((raw_host, raw_path)): AxumPath<(String, String)>,
) -> Response {
    let host = match host_or_error(&state, &raw_host) {
        Ok(host) => host,
        Err(response) => return response,
    };
    if validate_absolute_path(&raw_path).is_err() {
        return api_error(
            StatusCode::BAD_REQUEST,
            "invalid_path",
            "preview path must be absolute",
        );
    }
    match state
        .request_host_file(
            &host.id,
            HostFileOperation::Read {
                path: raw_path,
                root_path: None,
                max_bytes: 25 * 1024 * 1024,
            },
        )
        .await
    {
        Err(error) => transport_error(error),
        Ok(HostFileOutcome::Content(content)) => file_response(content),
        Ok(HostFileOutcome::Failed { code, message }) => host_failure(&code, &message),
        Ok(_) => api_error(
            StatusCode::BAD_GATEWAY,
            "host_unavailable",
            "host returned an invalid preview response",
        ),
    }
}
