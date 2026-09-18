//! Batch B7: project workspace, attachments and thread sections.
//!
//! Fourteen bb routes over three loosely related surfaces:
//!
//! | route | what it answers |
//! | --- | --- |
//! | `projects.files` | the project workspace's files |
//! | `projects.paths` | the project workspace's files and/or directories |
//! | `projects.fileContent` | one file's bytes inside the workspace |
//! | `projects.commands` | the prompt commands the workspace makes available |
//! | `projects.attachmentContent` | one stored attachment's bytes |
//! | `projects.uploadAttachment` | store an uploaded file as an attachment |
//! | `projects.copyAttachments` | copy attachments from another project |
//! | `projects.promptHistory` | the project's threads' user prompts |
//! | `projects.reorder` | move a project in the sidebar order |
//! | `projects.updateSource` | repoint or repromote one project source |
//! | `projects.delete` | tombstone a project |
//! | `threadSections.create` / `update` / `delete` | maintain sidebar sections |
//!
//! # Files live on a host, not here
//!
//! Every file route resolves a project to a **source** — the machine that holds
//! its code — and asks that host through [`crate::host_files`]. The control
//! plane reads nothing from its own disk and calls it a project file; that is
//! the same rule B5 set for thread files, and it is why a project with no
//! enrolled source answers `404 not_found` instead of a
//! plausible-looking empty listing.
//!
//! Attachments are the other half of the same rule. An upload is stored under
//! the host's reported data directory
//! ([`project_attachments_root`]), written by a
//! [`HostFileOperation::Write`](loom_provider_protocol::HostFileOperation::Write)
//! the host confines to that root, so an upload cannot name an arbitrary path
//! on the machine. A host that never reported a data directory answers `501
//! not_configured` rather than inventing one.
//!
//! # Permission boundaries
//!
//! Four scopes, and they are not interchangeable:
//!
//! * **Workspace scope** (`files`, `paths`, `fileContent`) — the project
//!   source's own path, root-confined on the worker side.
//! * **Attachment scope** (`attachmentContent`, `uploadAttachment`,
//!   `copyAttachments`) — the project's attachment directory, root-confined on
//!   both sides of a copy.
//! * **Entity scope** (`promptHistory`, `reorder`, `updateSource`, `delete`,
//!   `threadSections.*`) — answered from loom's own registry, no host involved.
//! * **Host workspace scope** (`commands`) — resolved from a project source,
//!   but the answer is the host's filesystem, so it is a host RPC.
//!
//! Every client-supplied relative path is validated before a request is built:
//! NUL, a leading `/`, and any `.`/`..` segment are refused with `400
//! invalid_path`, exactly as bb's `parseSafeRelativeRoutePath` does. The worker
//! re-checks containment on the resolved path, which is the half the control
//! plane cannot do — and for an upload, checks the *parent* directory, because
//! the target file does not exist yet.
//!
//! # Thread sections
//!
//! A section is a durable entity now ([`loom_domain::ThreadSection`]), listed in
//! the sidebar bootstrap. Deleting one **counts** the threads that referenced it
//! and does not rewrite them: the thread's stored grouping is the client's last
//! write, and re-filing every thread would be an unrequested mutation of a
//! different entity. See `docs/projects.md`.

// Every handler here answers with `Response`, which is large enough that
// clippy's `result_large_err` fires on each helper returning one. See `b5.rs`
// for the same allow and the same reasoning.
#![allow(clippy::result_large_err)]

use std::path::Path;

use axum::body::Body;
use axum::extract::{Multipart, Path as AxumPath, Query, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use loom_domain::{Environment, MessageRole, Project, ProjectId, ProjectSourceId, ThreadSectionId};
use loom_provider_protocol::{
    project_attachments_root, HostFileContent, HostFileOperation, HostFileOutcome,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::b5::{base64_decode, base64_encode, validate_absolute_path, validate_relative_path};
use crate::host_files::HostFileTransportError;
use crate::state::AppState;

/// The largest file any project content route will serve.
///
/// The same 25 MB B5 uses for a thread file, and for the same reason: the
/// worker enforces it from the request, so there is one number rather than two
/// that can drift.
const MAX_FILE_CONTENT_BYTES: u64 = 25 * 1024 * 1024;

/// The largest single attachment loom will accept or copy.
///
/// Smaller than a workspace file on purpose. An attachment is re-read by every
/// client that renders the prompt that carries it, so the ceiling that matters
/// is the one a client can afford to hold, not the one a disk can.
pub const MAX_ATTACHMENT_BYTES: u64 = 16 * 1024 * 1024;

/// Default and maximum entries in a project file listing.
const FILE_LIST_LIMIT_DEFAULT: usize = 1_000;
const FILE_LIST_LIMIT_MAX: usize = 10_000;

/// The BB dialog limit on one attachment filename's length.
const MAX_ATTACHMENT_NAME_BYTES: usize = 255;

/* ------------------------------------------------------------------ */
/* Errors                                                              */
/* ------------------------------------------------------------------ */

fn api_error(status: StatusCode, code: &'static str, message: impl Into<String>) -> Response {
    (
        status,
        Json(json!({ "code": code, "message": message.into() })),
    )
        .into_response()
}

fn invalid_path_response() -> Response {
    api_error(StatusCode::BAD_REQUEST, "invalid_path", "Invalid file path")
}

fn project_not_found(project_id: &ProjectId) -> Response {
    api_error(
        StatusCode::NOT_FOUND,
        "project_not_found",
        format!("project {project_id} is not known"),
    )
}

fn not_configured(project_id: &ProjectId) -> Response {
    api_error(
        StatusCode::NOT_IMPLEMENTED,
        "not_configured",
        format!(
            "project {project_id} has no host that reported a data directory, so its \
             attachments cannot be located"
        ),
    )
}

/// Maps a host answer onto the contract's error vocabulary.
fn host_failure(code: &str, message: &str) -> Response {
    let (status, public_code) = match code {
        "not_found" | "path_not_found" => (StatusCode::NOT_FOUND, "not_found"),
        "invalid_path" | "workspace_type_mismatch" | "invalid_request" => {
            (StatusCode::BAD_REQUEST, "invalid_path")
        }
        "file_too_large" => (StatusCode::PAYLOAD_TOO_LARGE, "file_too_large"),
        "conflict" => (StatusCode::CONFLICT, "conflict"),
        "permission_denied" => (StatusCode::FORBIDDEN, "forbidden"),
        _ => (StatusCode::BAD_GATEWAY, "host_unavailable"),
    };
    api_error(status, public_code, message)
}

/// Maps a transport failure onto the contract's error vocabulary.
fn transport_error(error: HostFileTransportError) -> Response {
    match error {
        HostFileTransportError::Publish(message) => {
            api_error(StatusCode::BAD_GATEWAY, "host_unavailable", message)
        }
        HostFileTransportError::Timeout => api_error(
            StatusCode::GATEWAY_TIMEOUT,
            "command_timeout",
            "the host did not answer the file request in time",
        ),
        HostFileTransportError::Disconnected(message) => {
            api_error(StatusCode::BAD_GATEWAY, "host_unavailable", message)
        }
        HostFileTransportError::UnknownHost(message) => {
            api_error(StatusCode::NOT_FOUND, "host_not_found", message)
        }
    }
}

fn command_error(error: crate::CommandError) -> Response {
    crate::http::command_error_response(error)
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

/* ------------------------------------------------------------------ */
/* Project and host resolution                                         */
/* ------------------------------------------------------------------ */

/// A project that exists and is not deleted.
fn live_project(state: &AppState, raw_project_id: &str) -> Result<Project, Response> {
    let project_id = raw_project_id.parse::<ProjectId>().map_err(|error| {
        api_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            error.to_string(),
        )
    })?;
    let project = state
        .registry
        .project(&project_id)
        .ok_or_else(|| project_not_found(&project_id))?;
    if project.is_deleted() {
        return Err(project_not_found(&project_id));
    }
    Ok(project)
}

/// The environment a route was told to read through, when it named one.
fn named_environment(state: &AppState, raw_id: &str) -> Result<Environment, Response> {
    let environment_id = raw_id
        .parse::<loom_domain::EnvironmentId>()
        .map_err(|error| {
            api_error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                error.to_string(),
            )
        })?;
    state.registry.environment(&environment_id).ok_or_else(|| {
        api_error(
            StatusCode::NOT_FOUND,
            "environment_not_found",
            format!("environment {environment_id} is not known"),
        )
    })
}

/// Which host and local path a project's file routes act on.
struct ProjectWorkspace {
    host_id: loom_domain::HostId,
    path: String,
}

/// Resolves the workspace a project's file routes read.
///
/// The contract's query parameters make this three-way, and the order is the
/// decision:
///
/// 1. `environmentId` names an environment; its host and path win, because a
///    client that opened a specific environment (a worktree, a checkout with a
///    different branch) means *that* workspace, not the project's default one.
/// 2. Otherwise `hostId` picks the source on that machine, so a multi-host
///    project can be read on the machine the client is looking at.
/// 3. Otherwise the project's **default** source is used, which is the source a
///    workspace is provisioned from. A project with sources therefore always
///    resolves; a project with none is a `404`.
///
/// A resolved source with an empty path is refused (`409 workspace_not_ready`)
/// rather than answered with an empty listing: a remote-only source declares a
/// repository that has no checkout yet, and an empty directory is a different
/// fact.
fn project_workspace(
    state: &AppState,
    project: &Project,
    environment_id: Option<&str>,
    host_id: Option<&str>,
) -> Result<ProjectWorkspace, Response> {
    if let Some(raw) = environment_id.filter(|raw| !raw.is_empty()) {
        let environment = named_environment(state, raw)?;
        if environment.project_id != project.id {
            return Err(api_error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                format!(
                    "environment {} belongs to project {}, not {}",
                    environment.id, environment.project_id, project.id
                ),
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
        return Ok(ProjectWorkspace {
            host_id: environment.host_id,
            path,
        });
    }

    let source = match host_id.filter(|raw| !raw.is_empty()) {
        Some(raw) => {
            let host_id = raw.parse::<loom_domain::HostId>().map_err(|error| {
                api_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request",
                    error.to_string(),
                )
            })?;
            project
                .sources
                .iter()
                .find(|source| source.host_id == host_id)
                .ok_or_else(|| {
                    // The contract has no `project_source_not_found` code; a
                    // project with no source on the named host is simply not
                    // found for the address the client used.
                    api_error(
                        StatusCode::NOT_FOUND,
                        "not_found",
                        format!("project {} has no source on host {host_id}", project.id),
                    )
                })?
        }
        None => project
            .sources
            .iter()
            .find(|source| source.is_default)
            // A registry written by an older build could hold sources with no
            // default; the first one is then the only honest answer.
            .or_else(|| project.sources.first())
            .ok_or_else(|| {
                api_error(
                    StatusCode::NOT_FOUND,
                    "not_found",
                    format!("project {} has no source", project.id),
                )
            })?,
    };
    if source.path.trim().is_empty() {
        // The contract has no `workspace_not_ready`; a source with no checkout
        // is a state the client has to resolve before the route can answer.
        return Err(api_error(
            StatusCode::CONFLICT,
            "conflict",
            "the project source has no checked-out path",
        ));
    }
    Ok(ProjectWorkspace {
        host_id: source.host_id.clone(),
        path: source.path.clone(),
    })
}

/// The host and directory a project's attachments live in.
///
/// The attachment directory belongs to a host's data directory, so the machine
/// is chosen the same way a workspace is — explicitly by `hostId`, else the
/// default source's — and a host that never reported a data directory is a
/// `501` rather than a guessed path.
fn attachment_target(
    state: &AppState,
    project: &Project,
    host_id: Option<&str>,
) -> Result<(loom_domain::HostId, String), Response> {
    let host_id = match host_id.filter(|raw| !raw.is_empty()) {
        Some(raw) => raw.parse::<loom_domain::HostId>().map_err(|error| {
            api_error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                error.to_string(),
            )
        })?,
        None => project
            .sources
            .iter()
            .find(|source| source.is_default)
            .or_else(|| project.sources.first())
            .map(|source| source.host_id.clone())
            .ok_or_else(|| {
                api_error(
                    StatusCode::NOT_FOUND,
                    "not_found",
                    format!("project {} has no source", project.id),
                )
            })?,
    };
    let host = state.registry.host(&host_id).ok_or_else(|| {
        api_error(
            StatusCode::NOT_FOUND,
            "host_not_found",
            format!("host {host_id} is not enrolled on this server"),
        )
    })?;
    let Some(data_dir) = host.data_dir.clone() else {
        return Err(not_configured(&project.id));
    };
    let root = project_attachments_root(&data_dir, &project.id.to_string());
    Ok((host_id, root))
}

/* ------------------------------------------------------------------ */
/* Query and body types                                                */
/* ------------------------------------------------------------------ */

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectFilesQuery {
    #[serde(default)]
    environment_id: Option<String>,
    #[serde(default)]
    host_id: Option<String>,
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    limit: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectPathsQuery {
    #[serde(default)]
    environment_id: Option<String>,
    #[serde(default)]
    host_id: Option<String>,
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    limit: Option<String>,
    /// Required by the contract: a client states which kinds it wants.
    include_files: String,
    include_directories: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectFileContentQuery {
    #[serde(default)]
    environment_id: Option<String>,
    #[serde(default)]
    host_id: Option<String>,
    path: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachmentContentQuery {
    path: String,
    #[serde(default)]
    host_id: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectCommandsQuery {
    provider: String,
    #[serde(default)]
    environment_id: Option<String>,
    #[serde(default)]
    host_id: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptHistoryQuery {
    #[serde(default)]
    limit: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReorderProjectRequest {
    previous_project_id: Option<String>,
    next_project_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateSourceRequest {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    is_default: bool,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CopyAttachmentsRequest {
    source_project_id: String,
    paths: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct CreateSectionRequest {
    name: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct SectionIdRequest {
    id: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct UpdateSectionRequest {
    id: String,
    name: String,
}

/* ------------------------------------------------------------------ */
/* Listing and content helpers                                         */
/* ------------------------------------------------------------------ */

fn parse_limit(raw: Option<&String>, default: usize, maximum: usize) -> Result<usize, Response> {
    let Some(raw) = raw.filter(|raw| !raw.is_empty()) else {
        return Ok(default);
    };
    if !raw.bytes().all(|byte| byte.is_ascii_digit()) {
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
    if parsed == 0 {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "limit must be a positive integer",
        ));
    }
    Ok(parsed.min(maximum))
}

fn bool_query(raw: &str, field: &str) -> Result<bool, Response> {
    match raw {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(api_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            format!("{field} must be true or false"),
        )),
    }
}

/// Joins a root and a validated relative path with `/` separators.
///
/// The same concatenation B5 uses, for the same reason: the root is a path
/// string reported by another machine, and normalising it here would apply this
/// platform's path rules to that platform's filesystem.
fn join_root(root: &str, relative: &str) -> String {
    format!("{}/{}", root.trim_end_matches(['/', '\\']), relative)
}

/// A listing entry in bb's `fileSchema` shape (`{path, name}`).
fn file_entry_value(entry: &loom_provider_protocol::HostFileEntry) -> Value {
    json!({ "path": entry.path, "name": entry.name })
}

/// A listing entry in bb's `pathSchema` shape, which adds its kind and score.
fn path_entry_value(entry: &loom_provider_protocol::HostFileEntry) -> Value {
    let kind = match entry.kind {
        loom_provider_protocol::HostPathKind::File => "file",
        loom_provider_protocol::HostPathKind::Directory => "directory",
    };
    json!({
        "kind": kind,
        "path": entry.path,
        "name": entry.name,
        "score": entry.score,
        "positions": entry.positions,
    })
}

/// One file's bytes, with the headers a browser needs to render them safely.
fn file_response(content: HostFileContent) -> Response {
    let bytes = match content.content_encoding {
        loom_provider_protocol::HostFileEncoding::Utf8 => content.content.into_bytes(),
        loom_provider_protocol::HostFileEncoding::Base64 => match base64_decode(&content.content) {
            Some(bytes) => bytes,
            None => {
                return api_error(
                    StatusCode::BAD_GATEWAY,
                    "host_unavailable",
                    "the host returned malformed base64 content",
                )
            }
        },
    };
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    if let Some(mime) = content.mime_type.as_deref() {
        if let Ok(value) = HeaderValue::from_str(mime) {
            headers.insert(header::CONTENT_TYPE, value);
        }
    }
    if let Ok(value) = HeaderValue::from_str(&bytes.len().to_string()) {
        headers.insert(header::CONTENT_LENGTH, value);
    }
    (StatusCode::OK, headers, Body::from(bytes)).into_response()
}

fn content_from_outcome(outcome: HostFileOutcome) -> Response {
    match outcome {
        HostFileOutcome::Content(content) => file_response(content),
        HostFileOutcome::Failed { code, message } => host_failure(&code, &message),
        HostFileOutcome::Listing { .. } => api_error(
            StatusCode::BAD_GATEWAY,
            "host_unavailable",
            "the host answered a content request with a listing",
        ),
        HostFileOutcome::Written(_) | HostFileOutcome::Copied { .. } => api_error(
            StatusCode::BAD_GATEWAY,
            "host_unavailable",
            "the host answered a content request with a write result",
        ),
        HostFileOutcome::FileMetadata { .. }
        | HostFileOutcome::Conflict { .. }
        | HostFileOutcome::Done => api_error(
            StatusCode::BAD_GATEWAY,
            "host_unavailable",
            "the host answered a content request with a non-content result",
        ),
    }
}

fn listing_from_outcome(outcome: HostFileOutcome, files: bool) -> Response {
    match outcome {
        HostFileOutcome::Listing { entries, truncated } => {
            let projected = entries
                .iter()
                .map(|entry| {
                    if files {
                        file_entry_value(entry)
                    } else {
                        path_entry_value(entry)
                    }
                })
                .collect::<Vec<_>>();
            if files {
                Json(json!({ "files": projected, "truncated": truncated })).into_response()
            } else {
                Json(json!({ "paths": projected, "truncated": truncated })).into_response()
            }
        }
        HostFileOutcome::Failed { code, message } => host_failure(&code, &message),
        HostFileOutcome::Content(_) | HostFileOutcome::Written(_) => api_error(
            StatusCode::BAD_GATEWAY,
            "host_unavailable",
            "the host answered a listing request with content",
        ),
        HostFileOutcome::Copied { .. }
        | HostFileOutcome::FileMetadata { .. }
        | HostFileOutcome::Conflict { .. }
        | HostFileOutcome::Done => api_error(
            StatusCode::BAD_GATEWAY,
            "host_unavailable",
            "the host answered a listing request with a non-listing result",
        ),
    }
}

/// The prompt-input rows a project's prompt history returns.
///
/// bb's `promptHistoryEntrySchema` carries the input rows the client sent, and
/// loom stores a prompt's text; mentions are always empty because loom does not
/// resolve `@` references yet, so reporting none is the truth rather than a
/// dropped field. The same shape `threads.promptHistory` uses.
fn prompt_input_rows(text: &str) -> Value {
    json!([{ "type": "text", "text": text, "mentions": [] }])
}

/* ------------------------------------------------------------------ */
/* Project workspace file routes                                       */
/* ------------------------------------------------------------------ */

/// `projects.files`: the project workspace's files.
pub async fn project_files(
    State(state): State<AppState>,
    AxumPath(raw_project_id): AxumPath<String>,
    Query(query): Query<ProjectFilesQuery>,
) -> Response {
    let project = match live_project(&state, &raw_project_id) {
        Ok(project) => project,
        Err(response) => return response,
    };
    if let Some(raw) = query.query.as_deref() {
        if !(1..=256).contains(&raw.chars().count()) {
            return api_error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "query must contain between one and 256 characters",
            );
        }
    }
    let limit = match parse_limit(
        query.limit.as_ref(),
        FILE_LIST_LIMIT_DEFAULT,
        FILE_LIST_LIMIT_MAX,
    ) {
        Ok(limit) => limit,
        Err(response) => return response,
    };
    let workspace = match project_workspace(
        &state,
        &project,
        query.environment_id.as_deref(),
        query.host_id.as_deref(),
    ) {
        Ok(workspace) => workspace,
        Err(response) => return response,
    };
    let outcome = match state
        .request_host_file(
            &workspace.host_id,
            HostFileOperation::List {
                path: workspace.path,
                query: query.query,
                limit,
                include_files: true,
                // bb's `projects.files` is a file picker: directories are not
                // what it renders, and `projects.paths` is the route for both.
                include_directories: false,
                include_hidden: false,
            },
        )
        .await
    {
        Ok(outcome) => outcome,
        Err(error) => return transport_error(error),
    };
    listing_from_outcome(outcome, true)
}

/// `projects.paths`: the project workspace's files and/or directories.
pub async fn project_paths(
    State(state): State<AppState>,
    AxumPath(raw_project_id): AxumPath<String>,
    Query(query): Query<ProjectPathsQuery>,
) -> Response {
    let project = match live_project(&state, &raw_project_id) {
        Ok(project) => project,
        Err(response) => return response,
    };
    let include_files = match bool_query(&query.include_files, "includeFiles") {
        Ok(value) => value,
        Err(response) => return response,
    };
    let include_directories = match bool_query(&query.include_directories, "includeDirectories") {
        Ok(value) => value,
        Err(response) => return response,
    };
    if !include_files && !include_directories {
        return api_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "At least one path kind must be included",
        );
    }
    if let Some(raw) = query.query.as_deref() {
        if !(1..=256).contains(&raw.chars().count()) {
            return api_error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "query must contain between one and 256 characters",
            );
        }
    }
    let limit = match parse_limit(
        query.limit.as_ref(),
        FILE_LIST_LIMIT_DEFAULT,
        FILE_LIST_LIMIT_MAX,
    ) {
        Ok(limit) => limit,
        Err(response) => return response,
    };
    let workspace = match project_workspace(
        &state,
        &project,
        query.environment_id.as_deref(),
        query.host_id.as_deref(),
    ) {
        Ok(workspace) => workspace,
        Err(response) => return response,
    };
    let outcome = match state
        .request_host_file(
            &workspace.host_id,
            HostFileOperation::List {
                path: workspace.path,
                query: query.query,
                limit,
                include_files,
                include_directories,
                include_hidden: false,
            },
        )
        .await
    {
        Ok(outcome) => outcome,
        Err(error) => return transport_error(error),
    };
    listing_from_outcome(outcome, false)
}

/// `projects.fileContent`: a root-relative path's bytes inside the workspace.
pub async fn project_file_content(
    State(state): State<AppState>,
    AxumPath(raw_project_id): AxumPath<String>,
    Query(query): Query<ProjectFileContentQuery>,
) -> Response {
    let project = match live_project(&state, &raw_project_id) {
        Ok(project) => project,
        Err(response) => return response,
    };
    if validate_relative_path(&query.path).is_err() {
        return invalid_path_response();
    }
    let workspace = match project_workspace(
        &state,
        &project,
        query.environment_id.as_deref(),
        query.host_id.as_deref(),
    ) {
        Ok(workspace) => workspace,
        Err(response) => return response,
    };
    let path = join_root(&workspace.path, &query.path);
    let outcome = match state
        .request_host_file(
            &workspace.host_id,
            HostFileOperation::Read {
                path,
                root_path: Some(workspace.path),
                max_bytes: MAX_FILE_CONTENT_BYTES,
            },
        )
        .await
    {
        Ok(outcome) => outcome,
        Err(error) => return transport_error(error),
    };
    content_from_outcome(outcome)
}

/// `projects.commands`: the prompt commands a project workspace exposes.
///
/// A command list is a property of the workspace on disk, so it is a host RPC
/// against the project's source. The `provider` parameter is required by the
/// contract; loom runs one provider, so a value naming a different one is a
/// `400` rather than a silently different answer.
pub async fn project_commands(
    State(state): State<AppState>,
    AxumPath(raw_project_id): AxumPath<String>,
    Query(query): Query<ProjectCommandsQuery>,
) -> Response {
    let project = match live_project(&state, &raw_project_id) {
        Ok(project) => project,
        Err(response) => return response,
    };
    let configured = state.provider_spec().name.clone();
    if query.provider != configured {
        return api_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            format!(
                "provider {:?} is not configured; this server runs {configured:?}",
                query.provider
            ),
        );
    }
    let workspace = match project_workspace(
        &state,
        &project,
        query.environment_id.as_deref(),
        query.host_id.as_deref(),
    ) {
        Ok(workspace) => workspace,
        Err(response) => return response,
    };
    let outcome = match state
        .request_host_rpc(
            &workspace.host_id,
            loom_provider_protocol::HostRpcOperation::ListCommands {
                cwd: workspace.path,
            },
        )
        .await
    {
        Ok(outcome) => outcome,
        Err(error) => {
            use crate::host_rpc::HostRpcTransportError;
            return match error {
                HostRpcTransportError::Publish(message) => {
                    api_error(StatusCode::BAD_GATEWAY, "host_unavailable", message)
                }
                HostRpcTransportError::Timeout => api_error(
                    StatusCode::GATEWAY_TIMEOUT,
                    "command_timeout",
                    "the host did not answer the command listing in time",
                ),
                HostRpcTransportError::Disconnected(message) => {
                    api_error(StatusCode::BAD_GATEWAY, "host_unavailable", message)
                }
                HostRpcTransportError::UnknownHost(message) => {
                    api_error(StatusCode::NOT_FOUND, "host_not_found", message)
                }
            };
        }
    };
    match outcome {
        loom_provider_protocol::HostRpcOutcome::Result { result } => {
            match project_commands_projection(result) {
                Ok(body) => Json(body).into_response(),
                Err(response) => response,
            }
        }
        loom_provider_protocol::HostRpcOutcome::Failed { code, message } => match code.as_str() {
            "invalid_path" => api_error(StatusCode::BAD_REQUEST, "invalid_path", message),
            _ => api_error(StatusCode::BAD_GATEWAY, "host_unavailable", message),
        },
    }
}

/// Projects a host's raw command rows into bb's `projectCommandSchema`.
///
/// Every row is validated rather than passed through: the contract's schema is
/// `additionalProperties: false` with four required fields, and a host that
/// answered something else would otherwise produce a response the client cannot
/// parse.
fn project_commands_projection(result: Value) -> Result<Value, Response> {
    let commands = result
        .get("commands")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            api_error(
                StatusCode::BAD_GATEWAY,
                "host_unavailable",
                "the host omitted its command list",
            )
        })?;
    let mut projected = Vec::with_capacity(commands.len());
    for command in commands {
        let Some(name) = command.get("name").and_then(Value::as_str) else {
            continue;
        };
        let origin = match command.get("origin").and_then(Value::as_str) {
            Some("project") => "project",
            Some("user") => "user",
            // A built-in command is the agent's own, which the contract calls
            // `builtin`. Anything else the host invents is reported as such
            // rather than dropped: the command exists and a client can run it.
            _ => "builtin",
        };
        projected.push(json!({
            "name": name,
            "source": "command",
            "origin": origin,
            "description": command.get("description").cloned().unwrap_or(Value::Null),
            "argumentHint": command.get("argumentHint").cloned().unwrap_or(Value::Null),
        }));
    }
    Ok(json!({ "commands": projected }))
}

/* ------------------------------------------------------------------ */
/* Attachments                                                         */
/* ------------------------------------------------------------------ */

/// `projects.attachmentContent`: a stored attachment's bytes.
///
/// The `path` parameter is the **host-local path an upload returned**, so the
/// worker reports whether it was inside the project's attachment root rather
/// than the control plane guessing. A path the host refuses is a `400`, which
/// is exactly what the containment check is for.
pub async fn project_attachment_content(
    State(state): State<AppState>,
    AxumPath(raw_project_id): AxumPath<String>,
    Query(query): Query<AttachmentContentQuery>,
) -> Response {
    let project = match live_project(&state, &raw_project_id) {
        Ok(project) => project,
        Err(response) => return response,
    };
    if validate_absolute_path(&query.path).is_err() {
        return invalid_path_response();
    }
    let (host_id, root) = match attachment_target(&state, &project, query.host_id.as_deref()) {
        Ok(target) => target,
        Err(response) => return response,
    };
    let outcome = match state
        .request_host_file(
            &host_id,
            HostFileOperation::Read {
                path: query.path,
                root_path: Some(root),
                max_bytes: MAX_ATTACHMENT_BYTES,
            },
        )
        .await
    {
        Ok(outcome) => outcome,
        Err(error) => return transport_error(error),
    };
    content_from_outcome(outcome)
}

/// `projects.uploadAttachment`: store an uploaded file as an attachment.
///
/// **The body is not JSON.** The contract declares `source: "form"`, so the
/// runtime request validator is a no-op here and this handler owns the whole
/// validation surface: a missing field, a field over the size or length bound,
/// or a multipart body that cannot be parsed are all rejected here with the
/// contract's own codes.
///
/// The file name is reduced to its final path segment before it is used. A
/// client-supplied `../../etc/passwd` must not escape the attachment directory,
/// and the worker's root confinement is the second half of that defence rather
/// than the only half.
pub async fn project_upload_attachment(
    State(state): State<AppState>,
    AxumPath(raw_project_id): AxumPath<String>,
    mut multipart: Multipart,
) -> Response {
    let project = match live_project(&state, &raw_project_id) {
        Ok(project) => project,
        Err(response) => return response,
    };
    let mut upload: Option<UploadedFile> = None;
    loop {
        let field = match multipart.next_field().await {
            Ok(Some(field)) => field,
            Ok(None) => break,
            Err(error) => {
                return api_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request",
                    format!("multipart body could not be read: {error}"),
                )
            }
        };
        let name = field.name().unwrap_or_default().to_owned();
        let file_name = field.file_name().map(str::to_owned);
        let content_type = field.content_type().map(str::to_owned);
        let bytes = match field.bytes().await {
            Ok(bytes) => bytes,
            Err(error) => {
                return api_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request",
                    format!("multipart field {name:?} could not be read: {error}"),
                )
            }
        };
        match name.as_str() {
            "file" | "files" | "attachment" => {
                if upload.is_some() {
                    return api_error(
                        StatusCode::BAD_REQUEST,
                        "invalid_request",
                        "exactly one file may be uploaded per request",
                    );
                }
                upload = Some(UploadedFile {
                    file_name: file_name.unwrap_or_else(|| "attachment".into()),
                    content_type,
                    bytes: bytes.to_vec(),
                });
            }
            "type" => {
                let raw = String::from_utf8_lossy(&bytes).trim().to_owned();
                if !matches!(raw.as_str(), "localFile" | "localImage") {
                    return api_error(
                        StatusCode::BAD_REQUEST,
                        "invalid_request",
                        "type must be localFile or localImage",
                    );
                }
            }
            // Any other part is ignored rather than rejected: a browser's
            // `FormData` carries the fields a client chose to send, and failing
            // the upload over an extra one would be brittle for no safety gain.
            _ => {}
        }
    }
    let Some(upload) = upload else {
        return api_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "no file part was present in the upload",
        );
    };
    if upload.bytes.is_empty() {
        return api_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "the uploaded file is empty",
        );
    }
    if upload.bytes.len() as u64 > MAX_ATTACHMENT_BYTES {
        return api_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "file_too_large",
            format!(
                "the upload is {} bytes, over the {} byte limit",
                upload.bytes.len(),
                MAX_ATTACHMENT_BYTES
            ),
        );
    }
    let Some(relative_name) = safe_attachment_name(&upload.file_name) else {
        return invalid_path_response();
    };
    let (host_id, root) = match attachment_target(&state, &project, None) {
        Ok(target) => target,
        Err(response) => return response,
    };
    let kind = if upload
        .content_type
        .as_deref()
        .is_some_and(|mime| mime.starts_with("image/"))
    {
        "localImage"
    } else {
        "localFile"
    };
    // A collision is not a failure: the worker suffixes the name, and the
    // response reports the path it actually wrote, which is what a client must
    // send back when it references the attachment.
    let target = join_root(&root, &relative_name);
    let content = base64_encode(&upload.bytes);
    let outcome = match state
        .request_host_file(
            &host_id,
            HostFileOperation::Write {
                path: target,
                root_path: root,
                content,
                max_bytes: MAX_ATTACHMENT_BYTES,
                overwrite: false,
            },
        )
        .await
    {
        Ok(outcome) => outcome,
        Err(error) => return transport_error(error),
    };
    let written = match outcome {
        HostFileOutcome::Written(written) => written,
        // A collision is not a failure: the worker suffixed the name, and the
        // response reports the path it actually wrote. So the only failures
        // that reach here are genuine (a bad path, an oversized file).
        HostFileOutcome::Failed { code, message } => return host_failure(&code, &message),
        _ => {
            return api_error(
                StatusCode::BAD_GATEWAY,
                "host_unavailable",
                "the host answered a write request with something else",
            )
        }
    };
    let size_bytes = written.size_bytes;
    let mime_type = written
        .mime_type
        .clone()
        .or_else(|| upload.content_type.clone());
    let name = Path::new(&written.path)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or(relative_name);
    (
        StatusCode::CREATED,
        Json(json!({
            "type": kind,
            "path": written.path,
            "name": name,
            "sizeBytes": size_bytes,
            "mimeType": mime_type,
        })),
    )
        .into_response()
}

struct UploadedFile {
    file_name: String,
    content_type: Option<String>,
    bytes: Vec<u8>,
}

/// Reduces a client-supplied file name to something safe to join under a root.
///
/// Only the final segment survives, and a name that reduces to nothing, to `.`
/// or to `..` is refused. This is the control-plane half of the traversal
/// defence; the worker re-checks the resolved parent against the root on the
/// machine that owns the filesystem.
fn safe_attachment_name(raw: &str) -> Option<String> {
    let name = raw
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(raw)
        .trim()
        .to_owned();
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.contains('\0')
        || name.len() > MAX_ATTACHMENT_NAME_BYTES
    {
        return None;
    }
    Some(name)
}

/// `projects.copyAttachments`: copy attachments in from another project.
///
/// The source directory is the **other project's** attachment root and the
/// destination is this project's, which is why the worker's copy operation
/// carries two roots. Both sides are confined on the machine that owns the
/// files, and a source that is missing, oversized or outside its root is
/// reported by the worker as a per-path failure the caller can act on.
pub async fn project_copy_attachments(
    State(state): State<AppState>,
    AxumPath(raw_project_id): AxumPath<String>,
    Json(request): Json<CopyAttachmentsRequest>,
) -> Response {
    let project = match live_project(&state, &raw_project_id) {
        Ok(project) => project,
        Err(response) => return response,
    };
    let source_project = match live_project(&state, &request.source_project_id) {
        Ok(project) => project,
        Err(response) => return response,
    };
    if request.paths.is_empty() || request.paths.len() > 100 {
        return api_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "between one and one hundred paths must be provided",
        );
    }
    for path in &request.paths {
        if validate_absolute_path(path).is_err() {
            return invalid_path_response();
        }
    }
    let (source_host, source_root) = match attachment_target(&state, &source_project, None) {
        Ok(target) => target,
        Err(response) => return response,
    };
    // A copy across two machines has no single filesystem to perform it on, and
    // pretending otherwise would silently move bytes through the control plane.
    // The client is told to upload instead, which is the supported path.
    let (destination_host, destination_root) = match attachment_target(&state, &project, None) {
        Ok(target) => target,
        Err(response) => return response,
    };
    if source_host != destination_host {
        return api_error(
            StatusCode::CONFLICT,
            "conflict",
            "the source and destination projects' attachments live on different hosts; \
             upload the files instead",
        );
    }
    let outcome = match state
        .request_host_file(
            &destination_host,
            HostFileOperation::Copy {
                paths: request.paths,
                source_root,
                destination: destination_root.clone(),
                destination_root,
                max_bytes: MAX_ATTACHMENT_BYTES,
            },
        )
        .await
    {
        Ok(outcome) => outcome,
        Err(error) => return transport_error(error),
    };
    match outcome {
        HostFileOutcome::Copied { files, failures } => {
            // The contract answers `{ok}` and the client watches the destination
            // for the files. A partial result is therefore still a success — the
            // copies that were made really exist — but a per-path failure is
            // never silent: a caller that asked for five files and got three
            // needs the two names, and the contract's error body has no place to
            // put them, so the log is where they go.
            for failure in &failures {
                eprintln!(
                    "loom-server: copying attachment {} from project {} failed: {} ({})",
                    failure.path, source_project.id, failure.message, failure.code
                );
            }
            let _ = files;
            Json(json!({ "ok": true })).into_response()
        }
        // The worker refuses the request as a whole only when the destination
        // itself is unusable, which is a real failure the client must see.
        HostFileOutcome::Failed { code, message } => host_failure(&code, &message),
        _ => api_error(
            StatusCode::BAD_GATEWAY,
            "host_unavailable",
            "the host answered a copy request with something else",
        ),
    }
}

/* ------------------------------------------------------------------ */
/* Prompt history, reorder, source update, delete                      */
/* ------------------------------------------------------------------ */

/// `projects.promptHistory`: the project's threads' user prompts, newest first.
///
/// Aggregated across the project's threads from the same `thread_message_added`
/// events `threads.promptHistory` reads, so the two routes cannot disagree
/// about what a prompt is. Newest first, because that is the order a
/// prompt-recall affordance walks.
pub async fn project_prompt_history(
    State(state): State<AppState>,
    AxumPath(raw_project_id): AxumPath<String>,
    Query(query): Query<PromptHistoryQuery>,
) -> Response {
    let project = match live_project(&state, &raw_project_id) {
        Ok(project) => project,
        Err(response) => return response,
    };
    let limit = match query.limit.as_deref().filter(|raw| !raw.is_empty()) {
        None => 50usize,
        Some(raw) => match raw.parse::<usize>() {
            Ok(parsed) if parsed > 0 => parsed.min(200),
            _ => {
                return api_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request",
                    "limit must be a positive integer",
                )
            }
        },
    };
    let mut prompts: Vec<(u64, Value)> = Vec::new();
    for thread in state
        .registry
        .threads()
        .into_iter()
        .filter(|thread| thread.project_id == project.id)
    {
        let entries = match crate::http::thread_domain_events(&state, &thread.id) {
            Ok(entries) => entries,
            Err(response) => return response,
        };
        for (_event_id, _sequence, created_at_ms, event) in entries {
            let loom_domain::DomainEvent::ThreadMessageAdded { message, .. } = event else {
                continue;
            };
            if message.role != MessageRole::User {
                continue;
            }
            prompts.push((
                message.created_at_ms,
                json!({
                    "id": message.id.to_string(),
                    "createdAt": message.created_at_ms,
                    "input": prompt_input_rows(&message.content),
                }),
            ));
            let _ = created_at_ms;
        }
    }
    prompts.sort_by_key(|entry| std::cmp::Reverse(entry.0));
    prompts.truncate(limit);
    Json(
        prompts
            .into_iter()
            .map(|(_created_at_ms, value)| value)
            .collect::<Vec<_>>(),
    )
    .into_response()
}

/// `projects.reorder`: move a project between two ranked neighbours.
///
/// Both neighbours are nullable, so the two ends of the list are expressible:
/// `previousProjectId: null` means "first", `nextProjectId: null` means "last".
/// A neighbour that is not in the current order, or a pair already in the
/// requested order, is a `409` rather than a silent no-op.
pub async fn project_reorder(
    State(state): State<AppState>,
    AxumPath(raw_project_id): AxumPath<String>,
    Json(request): Json<ReorderProjectRequest>,
) -> Response {
    let project = match live_project(&state, &raw_project_id) {
        Ok(project) => project,
        Err(response) => return response,
    };
    let previous = match parse_optional_project_id(request.previous_project_id.as_deref()) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let next = match parse_optional_project_id(request.next_project_id.as_deref()) {
        Ok(value) => value,
        Err(response) => return response,
    };
    match state.registry.reorder_project(
        &project.id,
        previous.as_ref(),
        next.as_ref(),
        loom_relay::now_ms(),
    ) {
        Ok((projects, events)) => {
            if let Err(response) = publish_events(&state, &events) {
                return response;
            }
            Json(
                projects
                    .iter()
                    .map(crate::http::project_value)
                    .collect::<Vec<_>>(),
            )
            .into_response()
        }
        Err(error) => command_error(error),
    }
}

fn parse_optional_project_id(raw: Option<&str>) -> Result<Option<ProjectId>, Response> {
    let Some(raw) = raw.filter(|raw| !raw.is_empty()) else {
        return Ok(None);
    };
    raw.parse::<ProjectId>().map(Some).map_err(|error| {
        api_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            error.to_string(),
        )
    })
}

/// `projects.updateSource`: repoint or repromote one project source.
///
/// The contract's request requires `type: "local_path"` and optionally carries
/// a new `path` and `isDefault: true`. `isDefault` is only ever `true`, so it
/// promotes the named source and steps the previous default down, maintaining
/// the one-default invariant the domain establishes.
pub async fn project_update_source(
    State(state): State<AppState>,
    AxumPath((raw_project_id, raw_source_id)): AxumPath<(String, String)>,
    Json(request): Json<UpdateSourceRequest>,
) -> Response {
    let project = match live_project(&state, &raw_project_id) {
        Ok(project) => project,
        Err(response) => return response,
    };
    if request.kind != "local_path" {
        return api_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "type must be local_path",
        );
    }
    let source_id = match raw_source_id.parse::<ProjectSourceId>() {
        Ok(source_id) => source_id,
        Err(error) => {
            return api_error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                error.to_string(),
            )
        }
    };
    if let Some(path) = request.path.as_deref() {
        if path.trim().is_empty() {
            return api_error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "path must not be blank",
            );
        }
    }
    let previous_default = project
        .sources
        .iter()
        .find(|source| source.is_default)
        .map(|source| source.id.clone());
    // A rename of the already-default source is a no-op on the flag, so this
    // only matters as documentation of the invariant the domain maintains.
    let _promoting = request.is_default && previous_default.as_ref() != Some(&source_id);
    match state.registry.update_project_source(
        &project.id,
        &source_id,
        request.path,
        request.is_default,
        loom_relay::now_ms(),
    ) {
        Ok(Some((updated, event))) => {
            if let Err(response) = publish_events(&state, &[event]) {
                return response;
            }
            match updated
                .sources
                .iter()
                .find(|source| source.id == source_id)
                .cloned()
            {
                Some(source) => Json(crate::http::project_source_value(&source)).into_response(),
                None => project_not_found(&project.id),
            }
        }
        // The source is not this project's. That is a `404`, not a silent
        // success: the caller named something that is not there.
        Ok(None) => api_error(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("source {source_id} is not part of project {}", project.id),
        ),
        Err(error) => command_error(error),
    }
}

/// `projects.delete`: tombstone a project.
///
/// Refuse, never cascade, exactly like archiving: a project with a live thread
/// or a live environment is a `409`. A second delete is a `404`, because the
/// project is already gone from the client's view.
pub async fn project_delete(
    State(state): State<AppState>,
    AxumPath(raw_project_id): AxumPath<String>,
) -> Response {
    let project = match live_project(&state, &raw_project_id) {
        Ok(project) => project,
        Err(response) => return response,
    };
    match state
        .registry
        .delete_project(&project.id, loom_relay::now_ms())
    {
        Ok((_project, event)) => match publish_events(&state, &[event]) {
            Ok(()) => Json(json!({ "ok": true })).into_response(),
            Err(response) => response,
        },
        Err(error) => command_error(error),
    }
}

/* ------------------------------------------------------------------ */
/* Thread sections                                                     */
/* ------------------------------------------------------------------ */

/// A section in bb's `threadSectionSchema` shape (`$defs/d743`).
pub(crate) fn section_value(section: &loom_domain::ThreadSection) -> Value {
    json!({
        "id": section.id.to_string(),
        "name": section.name,
        "createdAt": section.created_at_ms,
        "updatedAt": section.updated_at_ms,
    })
}

/// Maps a section command's failure onto the contract's section error codes.
///
/// The contract names these specifically (`section_not_found`,
/// `section_name_conflict`), so reporting the generic `not_found`/`conflict`
/// would be a truthful but less useful answer.
fn section_error(error: crate::CommandError) -> Response {
    match error {
        crate::CommandError::NotFound(message) => {
            api_error(StatusCode::NOT_FOUND, "section_not_found", message)
        }
        crate::CommandError::Conflict(message) => {
            api_error(StatusCode::CONFLICT, "section_name_conflict", message)
        }
        other => command_error(other),
    }
}

/// `threadSections.create`: a new sidebar section.
///
/// A name already in use is the contract's `409`. The status is `201` on
/// success.
pub async fn create_thread_section(
    State(state): State<AppState>,
    Json(request): Json<CreateSectionRequest>,
) -> Response {
    match state
        .registry
        .create_thread_section(request.name, loom_relay::now_ms())
    {
        Ok((section, event)) => match publish_events(&state, &[event]) {
            Ok(()) => (StatusCode::CREATED, Json(section_value(&section))).into_response(),
            Err(response) => response,
        },
        Err(error) => section_error(error),
    }
}

/// `threadSections.update`: rename a section.
pub async fn update_thread_section(
    State(state): State<AppState>,
    Json(request): Json<UpdateSectionRequest>,
) -> Response {
    let section_id = match request.id.parse::<ThreadSectionId>() {
        Ok(section_id) => section_id,
        Err(error) => {
            return api_error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                error.to_string(),
            )
        }
    };
    match state
        .registry
        .update_thread_section(&section_id, request.name, loom_relay::now_ms())
    {
        Ok((section, event)) => match publish_events(&state, &[event]) {
            Ok(()) => Json(json!({
                "id": section.id.to_string(),
                "name": section.name,
                // A rename touches no thread, so the count is zero. The
                // contract's update response reuses the delete shape, which
                // requires the field.
                "updatedThreadCount": 0,
            }))
            .into_response(),
            Err(response) => response,
        },
        Err(error) => section_error(error),
    }
}

/// `threadSections.delete`: remove a section.
///
/// The body carries the id, which is why this is a `DELETE` with a JSON body
/// rather than a path parameter. Threads that referenced the section are
/// counted, **not** rewritten; see the module docs.
pub async fn delete_thread_section(
    State(state): State<AppState>,
    Json(request): Json<SectionIdRequest>,
) -> Response {
    let section_id = match request.id.parse::<ThreadSectionId>() {
        Ok(section_id) => section_id,
        Err(error) => {
            return api_error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                error.to_string(),
            )
        }
    };
    match state
        .registry
        .delete_thread_section(&section_id, loom_relay::now_ms())
    {
        Ok((section, updated_thread_count, event)) => match publish_events(&state, &[event]) {
            Ok(()) => Json(json!({
                "id": section.id.to_string(),
                "name": section.name,
                "updatedThreadCount": updated_thread_count,
            }))
            .into_response(),
            Err(response) => response,
        },
        Err(error) => section_error(error),
    }
}
