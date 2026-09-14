//! Batch B6: environment lifecycle, workspace status/diff, and branch data.
//!
//! Environment workspace operations belong to the host that owns the
//! environment. HTTP handlers only resolve the domain binding and publish a
//! typed request through [`crate::host_rpc`]; the daemon performs the git work
//! and sends the result back over its enrolled socket.

#![allow(clippy::result_large_err)]

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use http_body_util::BodyExt;
use loom_domain::{Environment, EnvironmentId, EnvironmentStatus, HostId, ProjectId};
use loom_provider_protocol::{
    HostFileOperation, HostFileOutcome, HostPathKind, HostRpcOperation, HostRpcOutcome,
    WorkspaceContext, WorkspaceDiffFileSide, WorkspaceDiffTarget,
};
use serde::de::Deserializer;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::host_rpc::HostRpcTransportError;
use crate::state::AppState;
use crate::CommandError;

const DEFAULT_BRANCH_LIMIT: usize = 100;
const MAX_BRANCH_LIMIT: usize = 1_000;
const DEFAULT_PATH_LIMIT: usize = 1_000;
const MAX_PATH_LIMIT: usize = 10_000;
const MAX_DIFF_BYTES: u64 = 16 * 1024 * 1024;
const MAX_FILE_LIST_BYTES: u64 = 2 * 1024 * 1024;
const MAX_UNTRACKED_FILES: u64 = 1_000;
const MAX_UNTRACKED_LINE_STAT_FILES: u64 = 100;
const MAX_UNTRACKED_LINE_STAT_BYTES: u64 = 512 * 1024;
const COMMIT_MESSAGE: &str = "Commit workspace changes";

/* ------------------------------------------------------------------------- */
/* Common response and lookup helpers                                        */
/* ------------------------------------------------------------------------- */

fn api_error(status: StatusCode, code: &'static str, message: impl Into<String>) -> Response {
    (
        status,
        Json(json!({ "code": code, "message": message.into() })),
    )
        .into_response()
}

fn parse_environment_id(raw: &str) -> Result<EnvironmentId, Response> {
    raw.parse::<EnvironmentId>().map_err(|error| {
        api_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            error.to_string(),
        )
    })
}

fn parse_project_id(raw: &str) -> Result<ProjectId, Response> {
    raw.parse::<ProjectId>().map_err(|error| {
        api_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            error.to_string(),
        )
    })
}

fn parse_host_id(raw: &str) -> Result<HostId, Response> {
    raw.parse::<HostId>().map_err(|error| {
        api_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            error.to_string(),
        )
    })
}

fn environment(state: &AppState, raw_id: &str) -> Result<Environment, Response> {
    let environment_id = parse_environment_id(raw_id)?;
    state.registry.environment(&environment_id).ok_or_else(|| {
        api_error(
            StatusCode::NOT_FOUND,
            "environment_not_found",
            format!("environment {environment_id} is not known"),
        )
    })
}

fn workspace(state: &AppState, raw_id: &str) -> Result<(Environment, String), Response> {
    let environment = environment(state, raw_id)?;
    if environment.status == EnvironmentStatus::Destroyed {
        return Err(api_error(
            StatusCode::CONFLICT,
            "environment_not_ready",
            format!("environment {} is destroyed", environment.id),
        ));
    }
    let Some(path) = environment
        .path
        .clone()
        .filter(|path| !path.trim().is_empty())
    else {
        return Err(api_error(
            StatusCode::CONFLICT,
            "environment_not_ready",
            format!("environment {} has no workspace path", environment.id),
        ));
    };
    Ok((environment, path))
}

fn publish_events(state: &AppState, events: &[loom_domain::DomainEvent]) -> Result<(), Response> {
    crate::http::publish_all(state, events)
        .map(|_| ())
        .map_err(|error| {
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                error.to_string(),
            )
        })
}

fn command_error(error: CommandError) -> Response {
    crate::http::command_error_response(error)
}

fn transport_error(error: HostRpcTransportError) -> Response {
    match error {
        HostRpcTransportError::Publish(message) => {
            api_error(StatusCode::BAD_GATEWAY, "host_unavailable", message)
        }
        HostRpcTransportError::Timeout => api_error(
            StatusCode::GATEWAY_TIMEOUT,
            "command_timeout",
            "the host did not answer the workspace request in time",
        ),
        HostRpcTransportError::Disconnected(message) => {
            api_error(StatusCode::BAD_GATEWAY, "host_unavailable", message)
        }
        HostRpcTransportError::UnknownHost(message) => {
            api_error(StatusCode::NOT_FOUND, "host_not_found", message)
        }
    }
}

fn host_failure(code: &str, message: &str) -> Response {
    let (status, public_code) = match code {
        "path_not_found" => (StatusCode::NOT_FOUND, "path_not_found"),
        "invalid_path" => (StatusCode::BAD_REQUEST, "invalid_path"),
        "not_git_repo" => (StatusCode::CONFLICT, "not_git_repo"),
        "workspace_type_mismatch" => (StatusCode::BAD_REQUEST, "workspace_type_mismatch"),
        "permission_denied" => (StatusCode::FORBIDDEN, "permission_denied"),
        "unknown_environment" => (StatusCode::NOT_FOUND, "unknown_environment"),
        "no_changes" => (StatusCode::CONFLICT, "no_changes"),
        "pull_request_unavailable" => (StatusCode::CONFLICT, "pull_request_unavailable"),
        "pull_request_action_failed" => (StatusCode::CONFLICT, "pull_request_action_failed"),
        "file_too_large" => (StatusCode::PAYLOAD_TOO_LARGE, "file_too_large"),
        _ => (StatusCode::BAD_GATEWAY, "host_unavailable"),
    };
    api_error(status, public_code, message)
}

fn host_file_failure(code: &str, message: &str) -> Response {
    let (status, public_code) = match code {
        "not_found" | "path_not_found" => (StatusCode::NOT_FOUND, "not_found"),
        "invalid_path" | "workspace_type_mismatch" => (StatusCode::BAD_REQUEST, "invalid_path"),
        "file_too_large" => (StatusCode::PAYLOAD_TOO_LARGE, "file_too_large"),
        _ => (StatusCode::BAD_GATEWAY, "host_unavailable"),
    };
    api_error(status, public_code, message)
}

async fn request_workspace(
    state: &AppState,
    environment: &Environment,
    operation: HostRpcOperation,
) -> Result<HostRpcOutcome, Response> {
    state
        .request_host_rpc(&environment.host_id, operation)
        .await
        .map_err(transport_error)
}

fn context(path: &str) -> WorkspaceContext {
    WorkspaceContext {
        workspace_path: path.to_owned(),
    }
}

fn command_failure_code(code: &str) -> &'static str {
    match code {
        "path_not_found" => "path_not_found",
        "not_git_repo" => "not_git_repo",
        "workspace_type_mismatch" => "workspace_type_mismatch",
        "permission_denied" => "permission_denied",
        "unknown_environment" => "unknown_environment",
        _ => "unknown",
    }
}

/// Projects status-like workspace routes have a typed 200 response for both
/// a non-git directory and an unavailable host operation.
fn workspace_availability(outcome: HostRpcOutcome, workspace_path: &str) -> Response {
    match outcome {
        HostRpcOutcome::Result { result } => Json(result).into_response(),
        HostRpcOutcome::Failed { code, message } if code == "not_git_repo" => Json(json!({
            "outcome": "not_applicable",
            "reason": "non_git_environment",
            "message": message,
        }))
        .into_response(),
        HostRpcOutcome::Failed { code, message } => Json(json!({
            "outcome": "unavailable",
            "failure": {
                "code": command_failure_code(&code),
                "workspacePath": workspace_path,
                "message": message,
            },
        }))
        .into_response(),
    }
}

fn workspace_operation_failure(outcome: HostRpcOutcome) -> Response {
    match outcome {
        HostRpcOutcome::Result { result } => Json(result).into_response(),
        HostRpcOutcome::Failed { code, message } => host_failure(&code, &message),
    }
}

fn parse_limit(raw: Option<&str>, default: usize, maximum: usize) -> Result<usize, Response> {
    let Some(raw) = raw else {
        return Ok(default);
    };
    if raw.is_empty() || !raw.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "limit must be a non-negative integer",
        ));
    }
    let parsed = raw.parse::<usize>().map_err(|_| {
        api_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "limit is too large",
        )
    })?;
    Ok(parsed.clamp(1, maximum))
}

fn valid_hex_revision(raw: &str) -> bool {
    (4..=40).contains(&raw.len())
        && raw
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

fn valid_branch_reference(raw: &str) -> bool {
    !raw.is_empty()
        && raw.len() <= 4_096
        && raw != "@"
        && !raw.starts_with('-')
        && !raw
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
        && !raw.contains("..")
        && !raw.contains("@{")
        && !raw
            .bytes()
            .any(|byte| matches!(byte, b'~' | b'^' | b':' | b'?' | b'*' | b'[' | b'\\'))
        && !raw.starts_with('/')
        && !raw.ends_with('/')
        && !raw.contains("//")
        && raw.split('/').all(|component| {
            !component.is_empty()
                && component != "."
                && component != ".."
                && !component.starts_with('.')
                && !component.ends_with('.')
                && !component.ends_with(".lock")
        })
}

fn validate_search_query(raw: Option<&str>) -> Result<(), Response> {
    let Some(raw) = raw else {
        return Ok(());
    };
    let length = raw.chars().count();
    if !(1..=256).contains(&length) {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "query must contain between one and 256 characters",
        ));
    }
    Ok(())
}

fn validate_selected_branch(raw: Option<&str>) -> Result<(), Response> {
    let Some(raw) = raw else {
        return Ok(());
    };
    if !valid_branch_reference(raw) {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "selectedBranch must be a valid branch reference",
        ));
    }
    Ok(())
}

fn parse_diff_target(
    target: &str,
    merge_base_branch: Option<&str>,
    sha: Option<&str>,
) -> Result<WorkspaceDiffTarget, Response> {
    match target {
        "uncommitted" => Ok(WorkspaceDiffTarget::Uncommitted),
        "branch_committed" => {
            let branch = merge_base_branch.filter(|branch| valid_branch_reference(branch));
            branch
                .map(|merge_base_branch| WorkspaceDiffTarget::BranchCommitted {
                    merge_base_branch: merge_base_branch.to_owned(),
                })
                .ok_or_else(|| {
                    api_error(
                        StatusCode::BAD_REQUEST,
                        "invalid_request",
                        "branch_committed requires a valid mergeBaseBranch",
                    )
                })
        }
        "all" => {
            let branch = merge_base_branch.filter(|branch| valid_branch_reference(branch));
            branch
                .map(|merge_base_branch| WorkspaceDiffTarget::All {
                    merge_base_branch: merge_base_branch.to_owned(),
                })
                .ok_or_else(|| {
                    api_error(
                        StatusCode::BAD_REQUEST,
                        "invalid_request",
                        "all requires a valid mergeBaseBranch",
                    )
                })
        }
        "commit" => {
            let Some(sha) = sha.filter(|sha| valid_hex_revision(sha)) else {
                return Err(api_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request",
                    "commit requires a hexadecimal sha",
                ));
            };
            Ok(WorkspaceDiffTarget::Commit {
                sha: sha.to_owned(),
            })
        }
        _ => Err(api_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            format!("unknown diff target {target:?}"),
        )),
    }
}

fn validate_workspace_target(target: &WorkspaceDiffTarget) -> Result<(), Response> {
    match target {
        WorkspaceDiffTarget::Uncommitted => Ok(()),
        WorkspaceDiffTarget::BranchCommitted { merge_base_branch }
        | WorkspaceDiffTarget::All { merge_base_branch }
            if valid_branch_reference(merge_base_branch) =>
        {
            Ok(())
        }
        WorkspaceDiffTarget::Commit { sha } if valid_hex_revision(sha) => Ok(()),
        _ => Err(api_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "invalid diff target",
        )),
    }
}

fn valid_relative_path(raw: &str) -> bool {
    crate::b5::validate_relative_path(raw).is_ok()
}

/* ------------------------------------------------------------------------- */
/* Environment lifecycle routes                                              */
/* ------------------------------------------------------------------------- */

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateEnvironmentRequest {
    #[serde(default, deserialize_with = "double_option")]
    name: Option<Option<String>>,
    #[serde(default, deserialize_with = "double_option")]
    merge_base_branch: Option<Option<String>>,
}

fn double_option<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

pub async fn update_environment(
    State(state): State<AppState>,
    Path(raw_environment_id): Path<String>,
    Json(request): Json<UpdateEnvironmentRequest>,
) -> Response {
    let environment_id = match parse_environment_id(&raw_environment_id) {
        Ok(id) => id,
        Err(response) => return response,
    };
    if let Some(Some(branch)) = request.merge_base_branch.as_ref() {
        if !valid_branch_reference(branch) {
            return api_error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "mergeBaseBranch must be a valid branch reference",
            );
        }
    }
    match state.registry.update_environment(
        &environment_id,
        request.name,
        request.merge_base_branch,
        loom_relay::now_ms(),
    ) {
        Ok((environment, event)) => {
            if let Err(response) = publish_events(&state, &[event]) {
                return response;
            }
            Json(crate::http::environment_value(&environment)).into_response()
        }
        Err(error) => command_error(error),
    }
}

pub async fn archive_environment_threads(
    State(state): State<AppState>,
    Path(raw_environment_id): Path<String>,
) -> Response {
    let environment_id = match parse_environment_id(&raw_environment_id) {
        Ok(id) => id,
        Err(response) => return response,
    };
    if let Some(environment) = state.registry.environment(&environment_id) {
        if environment.status == EnvironmentStatus::Destroyed {
            return api_error(
                StatusCode::CONFLICT,
                "environment_not_ready",
                format!("environment {} is destroyed", environment.id),
            );
        }
    }
    match state
        .registry
        .archive_environment_threads(&environment_id, loom_relay::now_ms())
    {
        Ok((thread_ids, events)) => {
            if let Err(response) = publish_events(&state, &events) {
                return response;
            }
            Json(json!({
                "ok": true,
                "archivedThreadIds": thread_ids.iter().map(ToString::to_string).collect::<Vec<_>>(),
            }))
            .into_response()
        }
        Err(error) => command_error(error),
    }
}

pub async fn delete_environment(
    State(state): State<AppState>,
    Path(raw_environment_id): Path<String>,
) -> Response {
    let environment_id = match parse_environment_id(&raw_environment_id) {
        Ok(id) => id,
        Err(response) => return response,
    };
    match state
        .registry
        .delete_environment(&environment_id, loom_relay::now_ms())
    {
        Ok((_environment, event)) => {
            crate::b9::close_environment_terminals(&state, &environment_id);
            if let Some(event) = event {
                if let Err(response) = publish_events(&state, &[event]) {
                    return response;
                }
            }
            Json(json!({ "ok": true })).into_response()
        }
        Err(error) => command_error(error),
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct EnvironmentActionRequest {
    pub action: String,
    #[serde(default)]
    pub options: Option<EnvironmentActionOptions>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct EnvironmentActionOptions {
    pub method: String,
}

pub async fn environment_actions(
    State(state): State<AppState>,
    Path(raw_environment_id): Path<String>,
    Json(request): Json<EnvironmentActionRequest>,
) -> Response {
    let (environment, path) = match workspace(&state, &raw_environment_id) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let (operation, method, success_message) = match request.action.as_str() {
        "commit" => (
            HostRpcOperation::WorkspaceCommit {
                environment_id: environment.id.clone(),
                workspace_context: context(&path),
                message: COMMIT_MESSAGE.to_owned(),
            },
            None,
            "workspace changes committed",
        ),
        "pull_request_ready" | "pull_request_draft" => (
            HostRpcOperation::WorkspacePullRequestAction {
                environment_id: environment.id.clone(),
                workspace_context: context(&path),
                operation: request.action.clone(),
                method: None,
            },
            None,
            if request.action == "pull_request_ready" {
                "pull request marked ready"
            } else {
                "pull request marked draft"
            },
        ),
        "pull_request_merge" => {
            let Some(options) = request.options else {
                return api_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request",
                    "pull_request_merge requires a method",
                );
            };
            if !matches!(options.method.as_str(), "merge" | "squash" | "rebase") {
                return api_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request",
                    "method must be merge, squash, or rebase",
                );
            }
            (
                HostRpcOperation::WorkspacePullRequestAction {
                    environment_id: environment.id.clone(),
                    workspace_context: context(&path),
                    operation: request.action.clone(),
                    method: Some(options.method.clone()),
                },
                Some(options.method),
                "pull request merged",
            )
        }
        _ => {
            return api_error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                format!("unknown environment action {:?}", request.action),
            )
        }
    };

    let outcome = match request_workspace(&state, &environment, operation).await {
        Ok(outcome) => outcome,
        Err(response) => return response,
    };
    match outcome {
        HostRpcOutcome::Result { result } if request.action == "commit" => {
            let Some(commit_sha) = result.get("commitSha").and_then(Value::as_str) else {
                return api_error(
                    StatusCode::BAD_GATEWAY,
                    "host_unavailable",
                    "host returned an invalid commit result",
                );
            };
            let Some(commit_subject) = result.get("commitSubject").and_then(Value::as_str) else {
                return api_error(
                    StatusCode::BAD_GATEWAY,
                    "host_unavailable",
                    "host returned an invalid commit result",
                );
            };
            Json(json!({
                "ok": true,
                "action": "commit",
                "message": COMMIT_MESSAGE,
                "commitSha": commit_sha,
                "commitSubject": commit_subject,
            }))
            .into_response()
        }
        HostRpcOutcome::Result { .. } => {
            let mut response = json!({
                "ok": true,
                "action": request.action,
                "message": success_message,
            });
            if let Some(method) = method {
                response["method"] = Value::String(method);
            }
            Json(response).into_response()
        }
        HostRpcOutcome::Failed { code, message } => host_failure(&code, &message),
    }
}

/* ------------------------------------------------------------------------- */
/* Host filesystem path listing                                               */
/* ------------------------------------------------------------------------- */

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnvironmentPathsQuery {
    include_files: String,
    include_directories: String,
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    limit: Option<String>,
}

fn path_entry_value(entry: &loom_provider_protocol::HostFileEntry) -> Value {
    let kind = match entry.kind {
        HostPathKind::File => "file",
        HostPathKind::Directory => "directory",
    };
    json!({
        "kind": kind,
        "path": entry.path,
        "name": entry.name,
        "score": entry.score,
        "positions": entry.positions,
    })
}

pub async fn environment_paths(
    State(state): State<AppState>,
    Path(raw_environment_id): Path<String>,
    Query(query): Query<EnvironmentPathsQuery>,
) -> Response {
    if let Err(response) = validate_search_query(query.query.as_deref()) {
        return response;
    }
    let (environment, root) = match workspace(&state, &raw_environment_id) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let include_files = match query.include_files.as_str() {
        "true" => true,
        "false" => false,
        _ => {
            return api_error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "includeFiles must be true or false",
            )
        }
    };
    let include_directories = match query.include_directories.as_str() {
        "true" => true,
        "false" => false,
        _ => {
            return api_error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "includeDirectories must be true or false",
            )
        }
    };
    let limit = match parse_limit(query.limit.as_deref(), DEFAULT_PATH_LIMIT, MAX_PATH_LIMIT) {
        Ok(limit) => limit,
        Err(response) => return response,
    };
    let outcome = match state
        .request_host_file(
            &environment.host_id,
            HostFileOperation::List {
                path: root,
                query: query.query.filter(|query| !query.is_empty()),
                limit,
                include_files,
                include_directories,
                include_hidden: false,
            },
        )
        .await
    {
        Ok(outcome) => outcome,
        Err(error) => {
            return match error {
                crate::host_files::HostFileTransportError::Publish(message) => {
                    api_error(StatusCode::BAD_GATEWAY, "host_unavailable", message)
                }
                crate::host_files::HostFileTransportError::Timeout => api_error(
                    StatusCode::GATEWAY_TIMEOUT,
                    "command_timeout",
                    "the host did not answer the path request in time",
                ),
                crate::host_files::HostFileTransportError::Disconnected(message) => {
                    api_error(StatusCode::BAD_GATEWAY, "host_unavailable", message)
                }
                crate::host_files::HostFileTransportError::UnknownHost(message) => {
                    api_error(StatusCode::NOT_FOUND, "host_not_found", message)
                }
            }
        }
    };
    match outcome {
        HostFileOutcome::Listing { entries, truncated } => Json(json!({
            "paths": entries.iter().map(path_entry_value).collect::<Vec<_>>(),
            "truncated": truncated,
        }))
        .into_response(),
        HostFileOutcome::Failed { code, message } => host_file_failure(&code, &message),
        HostFileOutcome::Content(_) | HostFileOutcome::Written(_) => api_error(
            StatusCode::BAD_GATEWAY,
            "host_unavailable",
            "host answered a path request with file content",
        ),
        HostFileOutcome::Copied { .. }
        | HostFileOutcome::FileMetadata { .. }
        | HostFileOutcome::Conflict { .. }
        | HostFileOutcome::Done => api_error(
            StatusCode::BAD_GATEWAY,
            "host_unavailable",
            "host answered a path request with a non-listing result",
        ),
    }
}

/* ------------------------------------------------------------------------- */
/* Workspace status and diff routes                                           */
/* ------------------------------------------------------------------------- */

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceDiffQuery {
    target: String,
    #[serde(default)]
    merge_base_branch: Option<String>,
    #[serde(default)]
    sha: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceDiffFileQuery {
    target: String,
    #[serde(default)]
    merge_base_ref: Option<String>,
    #[serde(default)]
    sha: Option<String>,
    path: String,
    side: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct WorkspaceDiffPatchRequest {
    pub target: WorkspaceDiffTarget,
    pub paths: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceStatusQuery {
    #[serde(default)]
    merge_base_branch: Option<String>,
}

fn query_target(query: &WorkspaceDiffQuery) -> Result<WorkspaceDiffTarget, Response> {
    parse_diff_target(
        &query.target,
        query.merge_base_branch.as_deref(),
        query.sha.as_deref(),
    )
}

fn file_query_target(query: &WorkspaceDiffFileQuery) -> Result<WorkspaceDiffTarget, Response> {
    match query.target.as_str() {
        "uncommitted" => Ok(WorkspaceDiffTarget::Uncommitted),
        "branch_committed" | "all" => {
            let Some(merge_base_ref) = query
                .merge_base_ref
                .as_deref()
                .filter(|value| valid_hex_revision(value))
            else {
                return Err(api_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request",
                    "branch diff requires a hexadecimal mergeBaseRef",
                ));
            };
            if query.target == "branch_committed" {
                Ok(WorkspaceDiffTarget::BranchCommitted {
                    merge_base_branch: merge_base_ref.to_owned(),
                })
            } else {
                Ok(WorkspaceDiffTarget::All {
                    merge_base_branch: merge_base_ref.to_owned(),
                })
            }
        }
        "commit" => {
            let Some(sha) = query
                .sha
                .as_deref()
                .filter(|value| valid_hex_revision(value))
            else {
                return Err(api_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request",
                    "commit diff requires a hexadecimal sha",
                ));
            };
            Ok(WorkspaceDiffTarget::Commit {
                sha: sha.to_owned(),
            })
        }
        _ => Err(api_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            format!("unknown diff target {:?}", query.target),
        )),
    }
}

fn file_side(raw: &str) -> Result<WorkspaceDiffFileSide, Response> {
    match raw {
        "old" => Ok(WorkspaceDiffFileSide::Old),
        "new" => Ok(WorkspaceDiffFileSide::New),
        _ => Err(api_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "side must be old or new",
        )),
    }
}

pub async fn environment_status(
    State(state): State<AppState>,
    Path(raw_environment_id): Path<String>,
    Query(query): Query<WorkspaceStatusQuery>,
) -> Response {
    let (environment, path) = match workspace(&state, &raw_environment_id) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let merge_base_branch = match query.merge_base_branch {
        Some(branch) if valid_branch_reference(&branch) => Some(branch),
        Some(_) => {
            return api_error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "mergeBaseBranch must be a valid branch reference",
            )
        }
        None => environment.merge_base_branch.clone(),
    };
    let operation = HostRpcOperation::WorkspaceStatus {
        environment_id: environment.id.clone(),
        workspace_context: context(&path),
        merge_base_branch,
        max_untracked_line_stat_files: MAX_UNTRACKED_LINE_STAT_FILES,
        max_untracked_line_stat_bytes: MAX_UNTRACKED_LINE_STAT_BYTES,
    };
    let outcome = match request_workspace(&state, &environment, operation).await {
        Ok(outcome) => outcome,
        Err(response) => return response,
    };
    workspace_availability(outcome, &path)
}

pub async fn environment_diff(
    State(state): State<AppState>,
    Path(raw_environment_id): Path<String>,
    Query(query): Query<WorkspaceDiffQuery>,
) -> Response {
    let (environment, path) = match workspace(&state, &raw_environment_id) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let target = match query_target(&query) {
        Ok(target) => target,
        Err(response) => return response,
    };
    let operation = HostRpcOperation::WorkspaceDiff {
        environment_id: environment.id.clone(),
        workspace_context: context(&path),
        target,
        max_diff_bytes: MAX_DIFF_BYTES,
        max_file_list_bytes: MAX_FILE_LIST_BYTES,
        max_untracked_files: MAX_UNTRACKED_FILES,
    };
    let outcome = match request_workspace(&state, &environment, operation).await {
        Ok(outcome) => outcome,
        Err(response) => return response,
    };
    workspace_availability(outcome, &path)
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BranchOptionsQuery {
    #[serde(default)]
    limit: Option<String>,
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    selected_branch: Option<String>,
}

pub async fn environment_diff_branches(
    State(state): State<AppState>,
    Path(raw_environment_id): Path<String>,
    Query(query): Query<BranchOptionsQuery>,
) -> Response {
    if let Err(response) = validate_search_query(query.query.as_deref()) {
        return response;
    }
    if let Err(response) = validate_selected_branch(query.selected_branch.as_deref()) {
        return response;
    }
    let (environment, path) = match workspace(&state, &raw_environment_id) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let limit = match parse_limit(
        query.limit.as_deref(),
        DEFAULT_BRANCH_LIMIT,
        MAX_BRANCH_LIMIT,
    ) {
        Ok(limit) => limit,
        Err(response) => return response,
    };
    let operation = HostRpcOperation::ListBranchOptions {
        path,
        limit,
        query: query.query.filter(|query| !query.is_empty()),
        selected_branch: query.selected_branch.filter(|branch| !branch.is_empty()),
        remote_refresh: "none".into(),
    };
    let outcome = match state
        .request_host_rpc(&environment.host_id, operation)
        .await
    {
        Ok(outcome) => outcome,
        Err(error) => return transport_error(error),
    };
    workspace_operation_failure(outcome)
}

pub async fn environment_diff_files(
    State(state): State<AppState>,
    Path(raw_environment_id): Path<String>,
    Query(query): Query<WorkspaceDiffQuery>,
) -> Response {
    let (environment, path) = match workspace(&state, &raw_environment_id) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let target = match query_target(&query) {
        Ok(target) => target,
        Err(response) => return response,
    };
    let operation = HostRpcOperation::WorkspaceDiffFiles {
        environment_id: environment.id.clone(),
        workspace_context: context(&path),
        target,
        max_files: MAX_BRANCH_LIMIT as u64,
    };
    let outcome = match request_workspace(&state, &environment, operation).await {
        Ok(outcome) => outcome,
        Err(response) => return response,
    };
    workspace_availability(outcome, &path)
}

pub async fn environment_diff_patch(
    State(state): State<AppState>,
    Path(raw_environment_id): Path<String>,
    Json(request): Json<WorkspaceDiffPatchRequest>,
) -> Response {
    let (environment, path) = match workspace(&state, &raw_environment_id) {
        Ok(value) => value,
        Err(response) => return response,
    };
    if request.paths.is_empty() || request.paths.len() > 50 {
        return api_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "paths must contain between one and fifty entries",
        );
    }
    if request.paths.iter().any(|path| !valid_relative_path(path)) {
        return api_error(StatusCode::BAD_REQUEST, "invalid_path", "invalid diff path");
    }
    if let Err(response) = validate_workspace_target(&request.target) {
        return response;
    }
    let operation = HostRpcOperation::WorkspaceDiffPatch {
        environment_id: environment.id.clone(),
        workspace_context: context(&path),
        target: request.target,
        paths: request.paths,
        max_bytes_per_file: MAX_DIFF_BYTES,
    };
    let outcome = match request_workspace(&state, &environment, operation).await {
        Ok(outcome) => outcome,
        Err(response) => return response,
    };
    workspace_availability(outcome, &path)
}

pub async fn environment_diff_file(
    State(state): State<AppState>,
    Path(raw_environment_id): Path<String>,
    Query(query): Query<WorkspaceDiffFileQuery>,
) -> Response {
    let (environment, path) = match workspace(&state, &raw_environment_id) {
        Ok(value) => value,
        Err(response) => return response,
    };
    if !valid_relative_path(&query.path) {
        return api_error(StatusCode::BAD_REQUEST, "invalid_path", "invalid diff path");
    }
    let target = match file_query_target(&query) {
        Ok(target) => target,
        Err(response) => return response,
    };
    let side = match file_side(&query.side) {
        Ok(side) => side,
        Err(response) => return response,
    };
    let operation = HostRpcOperation::WorkspaceDiffFile {
        environment_id: environment.id.clone(),
        workspace_context: context(&path),
        target,
        path: query.path,
        side,
        max_bytes: MAX_DIFF_BYTES,
    };
    let outcome = match request_workspace(&state, &environment, operation).await {
        Ok(outcome) => outcome,
        Err(response) => return response,
    };
    workspace_operation_failure(outcome)
}

/* ------------------------------------------------------------------------- */
/* PR metadata and project branches                                           */
/* ------------------------------------------------------------------------- */

pub async fn environment_pull_request(
    State(state): State<AppState>,
    Path(raw_environment_id): Path<String>,
) -> Response {
    let (environment, path) = match workspace(&state, &raw_environment_id) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let operation = HostRpcOperation::WorkspacePullRequest {
        environment_id: environment.id.clone(),
        workspace_context: context(&path),
    };
    let outcome = match request_workspace(&state, &environment, operation).await {
        Ok(outcome) => outcome,
        Err(response) => {
            return match response.into_body().collect().await {
                Ok(body) => {
                    let body = body.to_bytes();
                    let message = serde_json::from_slice::<Value>(&body)
                        .ok()
                        .and_then(|value| {
                            value
                                .get("message")
                                .and_then(Value::as_str)
                                .map(str::to_owned)
                        })
                        .unwrap_or_else(|| "pull request metadata is unavailable".into());
                    Json(json!({ "outcome": "unavailable", "message": message })).into_response()
                }
                Err(_) => Json(json!({
                    "outcome": "unavailable",
                    "message": "pull request metadata is unavailable",
                }))
                .into_response(),
            }
        }
    };
    match outcome {
        HostRpcOutcome::Result { result } => Json(result).into_response(),
        HostRpcOutcome::Failed { message, .. } => {
            Json(json!({ "outcome": "unavailable", "message": message })).into_response()
        }
    }
}

fn project_source(
    state: &AppState,
    raw_project_id: &str,
    raw_host_id: &str,
) -> Result<(ProjectId, HostId, String), Response> {
    let project_id = parse_project_id(raw_project_id)?;
    let host_id = parse_host_id(raw_host_id)?;
    let project = state.registry.project(&project_id).ok_or_else(|| {
        api_error(
            StatusCode::NOT_FOUND,
            "project_not_found",
            format!("project {project_id} is not known"),
        )
    })?;
    let Some(source) = project
        .sources
        .iter()
        .find(|source| source.host_id == host_id)
    else {
        return Err(api_error(
            StatusCode::NOT_FOUND,
            "project_source_not_found",
            format!("project {project_id} has no source on host {host_id}"),
        ));
    };
    if source.path.trim().is_empty() {
        return Err(api_error(
            StatusCode::CONFLICT,
            "workspace_not_ready",
            "the project source has no checked-out path",
        ));
    }
    Ok((project_id, host_id, source.path.clone()))
}

fn branch_info_projection(result: Value, query: &BranchOptionsQuery) -> Result<Value, Response> {
    let Some(object) = result.as_object() else {
        return Err(api_error(
            StatusCode::BAD_GATEWAY,
            "host_unavailable",
            "host returned an invalid branch result",
        ));
    };
    let all_branches = object
        .get("branches")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            api_error(
                StatusCode::BAD_GATEWAY,
                "host_unavailable",
                "host omitted branches",
            )
        })?
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let all_remote_branches = object
        .get("remoteBranches")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            api_error(
                StatusCode::BAD_GATEWAY,
                "host_unavailable",
                "host omitted remote branches",
            )
        })?
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let query_text = query
        .query
        .as_deref()
        .filter(|value| !value.is_empty())
        .map(str::to_ascii_lowercase);
    let matches = |name: &str| {
        query_text
            .as_deref()
            .is_none_or(|query| name.to_ascii_lowercase().contains(query))
    };
    let local_candidates = all_branches
        .iter()
        .filter(|name| matches(name))
        .cloned()
        .collect::<Vec<_>>();
    let remote_candidates = all_remote_branches
        .iter()
        .filter(|name| matches(name))
        .cloned()
        .collect::<Vec<_>>();
    let limit = parse_limit(
        query.limit.as_deref(),
        DEFAULT_BRANCH_LIMIT,
        MAX_BRANCH_LIMIT,
    )?;
    let local_truncated = local_candidates.len() > limit;
    let remote_truncated = remote_candidates.len() > limit;
    let selected = query
        .selected_branch
        .as_deref()
        .filter(|value| !value.is_empty())
        .map(|name| {
            let kind = if all_branches.iter().any(|candidate| candidate == name) {
                "local"
            } else if all_remote_branches
                .iter()
                .any(|candidate| candidate == name)
            {
                "remote"
            } else {
                "missing"
            };
            json!({ "name": name, "kind": kind })
        });
    let mut result = result;
    let object = result
        .as_object_mut()
        .expect("branch result was checked above");
    object.insert(
        "branches".into(),
        json!(local_candidates.into_iter().take(limit).collect::<Vec<_>>()),
    );
    object.insert("branchesTruncated".into(), Value::Bool(local_truncated));
    object.insert(
        "remoteBranches".into(),
        json!(remote_candidates
            .into_iter()
            .take(limit)
            .collect::<Vec<_>>()),
    );
    object.insert(
        "remoteBranchesTruncated".into(),
        Value::Bool(remote_truncated),
    );
    object.insert("selectedBranch".into(), selected.unwrap_or(Value::Null));
    Ok(result)
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectBranchesQuery {
    host_id: String,
    #[serde(default)]
    limit: Option<String>,
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    selected_branch: Option<String>,
}

pub async fn project_branches(
    State(state): State<AppState>,
    Path(raw_project_id): Path<String>,
    Query(query): Query<ProjectBranchesQuery>,
) -> Response {
    if let Err(response) = validate_search_query(query.query.as_deref()) {
        return response;
    }
    if let Err(response) = validate_selected_branch(query.selected_branch.as_deref()) {
        return response;
    }
    let (_project_id, host_id, path) = match project_source(&state, &raw_project_id, &query.host_id)
    {
        Ok(value) => value,
        Err(response) => return response,
    };
    let outcome = match state
        .request_host_rpc(
            &host_id,
            HostRpcOperation::InspectGitSource {
                path,
                remote_refresh: "none".into(),
            },
        )
        .await
    {
        Ok(outcome) => outcome,
        Err(error) => return transport_error(error),
    };
    match outcome {
        HostRpcOutcome::Result { result } => {
            let query = BranchOptionsQuery {
                limit: query.limit,
                query: query.query,
                selected_branch: query.selected_branch,
            };
            match branch_info_projection(result, &query) {
                Ok(result) => Json(result).into_response(),
                Err(response) => response,
            }
        }
        HostRpcOutcome::Failed { code, message } => host_failure(&code, &message),
    }
}

pub async fn project_branch_options(
    State(state): State<AppState>,
    Path(raw_project_id): Path<String>,
    Query(query): Query<ProjectBranchesQuery>,
) -> Response {
    project_branches(State(state), Path(raw_project_id), Query(query)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_targets_reject_option_like_revisions() {
        assert!(valid_branch_reference("main"));
        assert!(!valid_branch_reference("--output=/tmp/out"));
        assert!(!valid_branch_reference("feature..bad"));
        assert!(valid_hex_revision("deadbeef"));
        assert!(!valid_hex_revision("dea"));
        assert!(!valid_hex_revision("DEADBEEF"));
    }

    #[test]
    fn the_environment_update_body_distinguishes_omitted_and_null() {
        let omitted: UpdateEnvironmentRequest = serde_json::from_str("{}").unwrap();
        assert_eq!(omitted.name, None);
        let cleared: UpdateEnvironmentRequest = serde_json::from_str(r#"{"name":null}"#).unwrap();
        assert_eq!(cleared.name, Some(None));
        let set: UpdateEnvironmentRequest =
            serde_json::from_str(r#"{"mergeBaseBranch":"main"}"#).unwrap();
        assert_eq!(set.merge_base_branch, Some(Some("main".into())));
    }
}
