//! Batch B9: workspace file operations and terminal sessions.
//!
//! Seventeen bb routes. Eight of them operate on files that live on a **host**,
//! and nine drive a terminal whose process runs on a **host**. The control
//! plane does neither itself:
//!
//! ```text
//!   files.*     ── HostFileRequest ──▶ relay host:{id} ──▶ daemon
//!   terminals.* ── TerminalRequest ──▶ relay host:{id} ──▶ daemon
//! ```
//!
//! That is the property these handlers exist to preserve: a file read never
//! opens the server's own disk, and a terminal never becomes a server-side PTY.
//!
//! # File permission boundaries
//!
//! Three scopes, and they are not interchangeable:
//!
//! * **Root-confined** (`mkdir`, `write`, `move`, `remove`, `read`) — every
//!   path is re-checked against `rootPath` on the daemon, on the *resolved*
//!   path, so a symlink cannot leave the root. A request that names no root is
//!   refused by these routes rather than sent unbounded; the daemon would allow
//!   it, but the route's job is to require the boundary.
//! * **Absolute-path** (`list`, `listPaths`, `read` without a root, preview) —
//!   the client names a path it already knows. It is still executed on the
//!   thread/project's host and never on the server.
//! * **Preview capability** (`files.createPreview` belongs to B8) — a
//!   short-lived root-bound lease minted earlier; `files.*` does not widen it.
//!
//! # Terminal ownership
//!
//! A session names a target, and the target decides the host:
//!
//! * `thread` — the host of the thread's environment, and the session is closed
//!   when that environment goes away.
//! * `environment` — the host that owns the environment.
//! * `host_path` — the host named in the target itself.
//!
//! A thread or environment whose host is not connected is refused before
//! anything is published, because a request into a room nobody is in would only
//! time out.
//!
//! # Output is bounded on the daemon, not here
//!
//! `terminals.output` reads a window from a cursor. The daemon owns a bounded
//! ring per session and reports `truncated` when it dropped older chunks; this
//! handler never accumulates output, which is what keeps a `yes` from growing
//! the control plane's memory.

#![allow(clippy::result_large_err)]

use axum::extract::{Path as AxumPath, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use loom_domain::{Environment, HostId, Thread};
use loom_provider_protocol::{
    HostFileEncoding, HostFileOperation, HostFileOutcome, TerminalOperation, TerminalOutcome,
    TerminalSession, TerminalStart, TerminalStatus, TerminalTarget,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::host_files::HostFileTransportError;
use crate::state::AppState;
use crate::terminals::{TerminalSessions, TerminalTransportError};
use crate::{b5::validate_absolute_path, b5::validate_relative_path};

/// The largest file `files.read` will serve.
///
/// The same ceiling B5 uses for a content route. One bound is easier to reason
/// about than two, and the daemon enforces it from the request it receives.
pub const MAX_FILE_OPERATION_BYTES: u64 = 25 * 1024 * 1024;

/// The largest attachment/file a `files.write` may carry.
pub const MAX_WRITE_BYTES: u64 = 25 * 1024 * 1024;

/// Default and maximum entries in a file listing.
const FILE_LIST_LIMIT_DEFAULT: usize = 1_000;
/// The ceiling a client's `limit` is clamped to.
const FILE_LIST_LIMIT_MAX: usize = 10_000;

/// bb's terminal column/row bounds, mirrored so a request is refused here
/// rather than at the daemon after a round trip.
const MAX_TERMINAL_COLS: u16 = 500;
const MAX_TERMINAL_ROWS: u16 = 200;

/// The largest output window one read may answer.
const MAX_TAIL_BYTES: u64 = 4 * 1024 * 1024;
/// The default output window.
const DEFAULT_TAIL_BYTES: u64 = 256 * 1024;
/// The largest number of output chunks one read may answer.
const MAX_OUTPUT_CHUNKS: usize = 10_000;
/// The default number of chunks when a client names none.
const DEFAULT_OUTPUT_CHUNKS: usize = 1_000;

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

fn invalid_path() -> Response {
    api_error(StatusCode::BAD_REQUEST, "invalid_path", "Invalid file path")
}

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

fn terminal_transport_error(error: TerminalTransportError) -> Response {
    match error {
        TerminalTransportError::Publish(message) => {
            api_error(StatusCode::BAD_GATEWAY, "host_unavailable", message)
        }
        TerminalTransportError::Timeout => api_error(
            StatusCode::GATEWAY_TIMEOUT,
            "command_timeout",
            "the host did not answer the terminal request in time",
        ),
        TerminalTransportError::Disconnected(message) => {
            api_error(StatusCode::BAD_GATEWAY, "host_unavailable", message)
        }
        TerminalTransportError::UnknownHost(message) => {
            api_error(StatusCode::NOT_FOUND, "host_not_found", message)
        }
    }
}

/// Maps a daemon file failure onto the contract's status codes.
///
/// The daemon's codes are the vocabulary the HTTP layer already uses; an
/// unrecognised one becomes the generic `host_unavailable` rather than a code
/// no client can branch on.
fn host_file_failure(code: &str, message: &str) -> Response {
    let (status, public_code) = match code {
        "not_found" | "path_not_found" => (StatusCode::NOT_FOUND, "not_found"),
        "invalid_path" => (StatusCode::BAD_REQUEST, "invalid_path"),
        "invalid_request" => (StatusCode::BAD_REQUEST, "invalid_request"),
        "conflict" => (StatusCode::CONFLICT, "conflict"),
        "file_too_large" => (StatusCode::PAYLOAD_TOO_LARGE, "file_too_large"),
        "unsupported_media_type" => (StatusCode::UNSUPPORTED_MEDIA_TYPE, "unsupported_media_type"),
        "permission_denied" => (StatusCode::FORBIDDEN, "forbidden"),
        _ => (StatusCode::BAD_GATEWAY, "host_unavailable"),
    };
    api_error(status, public_code, message)
}

/// Maps a daemon terminal failure onto the contract's status codes.
fn terminal_failure(code: &str, message: &str) -> Response {
    let (status, public_code) = match code {
        "terminal_not_found" => (StatusCode::NOT_FOUND, "terminal_not_found"),
        "terminal_not_running" => (StatusCode::CONFLICT, "terminal_not_running"),
        "invalid_path" => (StatusCode::BAD_REQUEST, "invalid_path"),
        "invalid_request" => (StatusCode::BAD_REQUEST, "invalid_request"),
        _ => (StatusCode::BAD_GATEWAY, "host_unavailable"),
    };
    api_error(status, public_code, message)
}

/* ------------------------------------------------------------------ */
/* Request types                                                       */
/* ------------------------------------------------------------------ */

/// `files.list`
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListRequest {
    path: String,
    #[serde(default)]
    host_id: Option<String>,
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    limit: Option<u32>,
    #[serde(default)]
    include_hidden: Option<bool>,
    #[serde(default)]
    exclude_names: Option<Vec<String>>,
}

/// `files.listPaths`
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListPathsRequest {
    path: String,
    include_files: bool,
    include_directories: bool,
    #[serde(default)]
    host_id: Option<String>,
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    limit: Option<u32>,
    #[serde(default)]
    include_hidden: Option<bool>,
    #[serde(default)]
    exclude_names: Option<Vec<String>>,
}

/// `files.mkdir` and `files.remove`, which share the same shape.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PathRequest {
    path: String,
    #[serde(default)]
    host_id: Option<String>,
    #[serde(default)]
    root_path: Option<String>,
    #[serde(default)]
    recursive: Option<bool>,
}

/// `files.move`
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MoveRequest {
    source_path: String,
    destination_path: String,
    #[serde(default)]
    host_id: Option<String>,
    #[serde(default)]
    root_path: Option<String>,
}

/// `files.read`
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadRequest {
    path: String,
    #[serde(default)]
    host_id: Option<String>,
    #[serde(default)]
    root_path: Option<String>,
}

/// `files.write`
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WriteRequest {
    path: String,
    content: String,
    #[serde(default)]
    host_id: Option<String>,
    #[serde(default)]
    root_path: Option<String>,
    #[serde(default)]
    content_encoding: Option<String>,
    #[serde(default)]
    create_parents: Option<bool>,
    /// Tri-state optimistic-concurrency assertion.
    ///
    /// The contract declares `expectedSha256` as `["string", "null"]` and
    /// makes it optional, and the three states mean three different things:
    /// absent is "no check", `null` is "the file must not exist yet", and a
    /// string is "the file must hash to this". A plain `Option<String>`
    /// collapses the first two, which would turn a create-only write into an
    /// unconditional overwrite, so the presence of the key is what is captured.
    #[serde(default, deserialize_with = "deserialize_tristate")]
    expected_sha256: Option<Option<String>>,
    #[serde(default)]
    mode: Option<u32>,
}

/// Distinguishes an absent field from a field explicitly set to `null`.
#[allow(clippy::option_option)]
fn deserialize_tristate<'de, D>(deserializer: D) -> Result<Option<Option<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<String>::deserialize(deserializer)?;
    Ok(Some(value))
}

/// `terminals.list` query.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalListQuery {
    host_id: Option<String>,
    thread_id: Option<String>,
    environment_id: Option<String>,
    cwd: Option<String>,
}

/// `terminals.output` query.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalOutputQuery {
    since_seq: Option<String>,
    limit_chunks: Option<String>,
    tail_bytes: Option<String>,
}

/// The nested `start` object of `terminals.create`.
#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum StartRequest {
    Shell,
    Command { command: String },
}

/// The nested `target` object of `terminals.create`.
#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TargetRequest {
    Thread {
        #[serde(rename = "threadId")]
        thread_id: String,
    },
    Environment {
        #[serde(rename = "environmentId")]
        environment_id: String,
    },
    HostPath {
        #[serde(rename = "hostId")]
        host_id: String,
        #[serde(default)]
        cwd: Option<String>,
    },
}

/// `terminals.create`
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateTerminalRequest {
    cols: u16,
    rows: u16,
    target: TargetRequest,
    #[serde(default)]
    start: Option<StartRequest>,
    #[serde(default)]
    title: Option<String>,
}

/// `terminals.input`
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalInputRequest {
    data_base64: String,
}

/// `terminals.resize`
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalResizeRequest {
    cols: u16,
    rows: u16,
}

/// `terminals.close`
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalCloseRequest {
    mode: String,
    reason: String,
}

/// `terminals.update`
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalUpdateRequest {
    title: String,
}

/* ------------------------------------------------------------------ */
/* File target resolution                                              */
/* ------------------------------------------------------------------ */

/// The host a file operation runs on, and the root it is confined to.
struct FileScope {
    host_id: HostId,
    root: Option<String>,
}

/// Resolves the host and root for a file route.
///
/// The host comes from an explicit `hostId` or, when absent, from the primary
/// connected host — the same rule `files.createPreview` uses. A request that
/// names an unknown or disconnected host is refused before anything is
/// published.
fn file_scope(
    state: &AppState,
    host_id: Option<&str>,
    root: Option<&str>,
) -> Result<FileScope, Response> {
    let host = match host_id.filter(|raw| !raw.trim().is_empty()) {
        Some(raw) => {
            let id = raw.parse::<HostId>().map_err(|error| {
                api_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request",
                    error.to_string(),
                )
            })?;
            let Some(host) = state.registry.host(&id) else {
                return Err(api_error(
                    StatusCode::NOT_FOUND,
                    "host_not_found",
                    format!("host {id} is not known"),
                ));
            };
            host
        }
        None => state
            .registry
            .primary_host(state.local_host_id())
            .ok_or_else(|| {
                api_error(
                    StatusCode::CONFLICT,
                    "host_unavailable",
                    "no connected host is available",
                )
            })?,
    };
    let root = match root.filter(|raw| !raw.trim().is_empty()) {
        Some(root) => {
            if validate_absolute_path(root).is_err() {
                return Err(invalid_path());
            }
            Some(root.to_owned())
        }
        None => None,
    };
    Ok(FileScope {
        host_id: host.id,
        root,
    })
}

/// Resolves a requested path against a root, when one was given.
///
/// With a root, the path is treated as **root-relative** and validated as such;
/// without one, it must be absolute. That split is what keeps a traversal out of
/// the request: the daemon re-checks containment on the resolved path, but a
/// `..` segment never reaches it.
#[allow(clippy::result_large_err)]
fn resolve_path(path: &str, root: Option<&str>) -> Result<String, Response> {
    match root {
        Some(root) => {
            let relative = validate_relative_path(path).map_err(|_| invalid_path())?;
            Ok(crate::b5::join_root(root, relative))
        }
        None => {
            validate_absolute_path(path).map_err(|_| invalid_path())?;
            Ok(path.to_owned())
        }
    }
}

fn root_required(scope: &FileScope) -> Result<&str, Response> {
    scope.root.as_deref().ok_or_else(|| {
        api_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "this operation requires an absolute rootPath",
        )
    })
}

/* ------------------------------------------------------------------ */
/* files.* routes                                                      */
/* ------------------------------------------------------------------ */

/// `files.list`: the direct children of one directory.
pub async fn files_list(
    State(state): State<AppState>,
    Json(request): Json<ListRequest>,
) -> Response {
    let scope = match file_scope(&state, request.host_id.as_deref(), None) {
        Ok(scope) => scope,
        Err(response) => return response,
    };
    if validate_absolute_path(&request.path).is_err() {
        return invalid_path();
    }
    if request
        .exclude_names
        .as_ref()
        .is_some_and(|names| names.iter().any(|name| name.len() > 255 || name.is_empty()))
    {
        return api_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "excludeNames entries must be 1-255 characters",
        );
    }
    let limit = request
        .limit
        .map(|limit| (limit as usize).clamp(1, FILE_LIST_LIMIT_MAX))
        .unwrap_or(FILE_LIST_LIMIT_DEFAULT);
    let outcome = state
        .request_host_file(
            &scope.host_id,
            // A fuzzy `query` is a recursive search by construction — a
            // non-recursive listing cannot answer "where is this name" — so the
            // two shapes are chosen by whether one was asked for. Both return
            // files only, which is what `files.list` declares.
            match request.query.as_deref().filter(|query| !query.is_empty()) {
                Some(query) => HostFileOperation::List {
                    path: request.path.clone(),
                    query: Some(query.to_owned()),
                    limit,
                    include_files: true,
                    include_directories: false,
                    include_hidden: request.include_hidden.unwrap_or(false),
                },
                None => HostFileOperation::ListDirectory {
                    path: request.path.clone(),
                    include_files: true,
                    include_directories: false,
                    include_hidden: request.include_hidden.unwrap_or(false),
                    limit,
                },
            },
        )
        .await;
    match outcome {
        Err(error) => transport_error(error),
        Ok(HostFileOutcome::Listing { entries, truncated }) => {
            let excluded = request.exclude_names.unwrap_or_default();
            let entries = apply_exclusions(entries, &excluded);
            // `fileSchema` is `{path, name}` and `additionalProperties: false`,
            // so the kind is deliberately not projected here.
            Json(json!({
                "files": entries
                    .iter()
                    .map(|entry| json!({ "path": entry.path, "name": entry.name }))
                    .collect::<Vec<_>>(),
                "truncated": truncated,
            }))
            .into_response()
        }
        Ok(HostFileOutcome::Failed { code, message }) => host_file_failure(&code, &message),
        Ok(_) => api_error(
            StatusCode::BAD_GATEWAY,
            "host_unavailable",
            "the host answered a listing request with a non-listing result",
        ),
    }
}

/// `files.listPaths`: a recursive listing, with kind and match scores.
pub async fn files_list_paths(
    State(state): State<AppState>,
    Json(request): Json<ListPathsRequest>,
) -> Response {
    let scope = match file_scope(&state, request.host_id.as_deref(), None) {
        Ok(scope) => scope,
        Err(response) => return response,
    };
    if validate_absolute_path(&request.path).is_err() {
        return invalid_path();
    }
    if !request.include_files && !request.include_directories {
        return api_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "at least one of includeFiles or includeDirectories is required",
        );
    }
    let limit = request
        .limit
        .map(|limit| (limit as usize).clamp(1, FILE_LIST_LIMIT_MAX))
        .unwrap_or(FILE_LIST_LIMIT_DEFAULT);
    let outcome = state
        .request_host_file(
            &scope.host_id,
            HostFileOperation::List {
                path: request.path.clone(),
                query: request.query.clone(),
                limit,
                include_files: request.include_files,
                include_directories: request.include_directories,
                include_hidden: request.include_hidden.unwrap_or(false),
            },
        )
        .await;
    match outcome {
        Err(error) => transport_error(error),
        Ok(HostFileOutcome::Listing { entries, truncated }) => {
            let excluded = request.exclude_names.unwrap_or_default();
            let entries = apply_exclusions(entries, &excluded);
            Json(json!({
                "paths": entries
                    .iter()
                    .map(|entry| json!({
                        "kind": match entry.kind {
                            loom_provider_protocol::HostPathKind::File => "file",
                            loom_provider_protocol::HostPathKind::Directory => "directory",
                        },
                        "path": entry.path,
                        "name": entry.name,
                        "score": entry.score,
                        "positions": entry.positions,
                    }))
                    .collect::<Vec<_>>(),
                "truncated": truncated,
            }))
            .into_response()
        }
        Ok(HostFileOutcome::Failed { code, message }) => host_file_failure(&code, &message),
        Ok(_) => api_error(
            StatusCode::BAD_GATEWAY,
            "host_unavailable",
            "the host answered a path request with a non-listing result",
        ),
    }
}

/// Drops entries whose final name is excluded, then re-reports truncation.
fn apply_exclusions(
    entries: Vec<loom_provider_protocol::HostFileEntry>,
    exclude_names: &[String],
) -> Vec<loom_provider_protocol::HostFileEntry> {
    if exclude_names.is_empty() {
        return entries;
    }
    entries
        .into_iter()
        .filter(|entry| !exclude_names.iter().any(|name| name == &entry.name))
        .collect()
}

/// `files.mkdir`
pub async fn files_mkdir(
    State(state): State<AppState>,
    Json(request): Json<PathRequest>,
) -> Response {
    if request.path.trim().is_empty() {
        return invalid_path();
    }
    let scope = match file_scope(
        &state,
        request.host_id.as_deref(),
        request.root_path.as_deref(),
    ) {
        Ok(scope) => scope,
        Err(response) => return response,
    };
    let root = match root_required(&scope) {
        Ok(root) => root.to_owned(),
        Err(response) => return response,
    };
    let path = match resolve_path(&request.path, Some(&root)) {
        Ok(path) => path,
        Err(response) => return response,
    };
    let outcome = state
        .request_host_file(
            &scope.host_id,
            HostFileOperation::CreateDirectory {
                path,
                root_path: Some(root),
                recursive: request.recursive.unwrap_or(false),
            },
        )
        .await;
    match outcome {
        Err(error) => transport_error(error),
        Ok(HostFileOutcome::Done) => Json(json!({ "ok": true })).into_response(),
        Ok(HostFileOutcome::Failed { code, message }) => host_file_failure(&code, &message),
        Ok(_) => api_error(
            StatusCode::BAD_GATEWAY,
            "host_unavailable",
            "the host answered a mkdir request with an unexpected result",
        ),
    }
}

/// `files.move`
pub async fn files_move(
    State(state): State<AppState>,
    Json(request): Json<MoveRequest>,
) -> Response {
    if request.source_path.trim().is_empty() || request.destination_path.trim().is_empty() {
        return invalid_path();
    }
    let scope = match file_scope(
        &state,
        request.host_id.as_deref(),
        request.root_path.as_deref(),
    ) {
        Ok(scope) => scope,
        Err(response) => return response,
    };
    let root = match root_required(&scope) {
        Ok(root) => root.to_owned(),
        Err(response) => return response,
    };
    let source = match resolve_path(&request.source_path, Some(&root)) {
        Ok(path) => path,
        Err(response) => return response,
    };
    let destination = match resolve_path(&request.destination_path, Some(&root)) {
        Ok(path) => path,
        Err(response) => return response,
    };
    let outcome = state
        .request_host_file(
            &scope.host_id,
            HostFileOperation::Move {
                source_path: source,
                destination_path: destination,
                root_path: Some(root),
                // A move that silently replaced an existing file would be the
                // data loss the write route already refuses, so the default is
                // no-overwrite.
                overwrite: false,
            },
        )
        .await;
    match outcome {
        Err(error) => transport_error(error),
        Ok(HostFileOutcome::Done) => Json(json!({ "ok": true })).into_response(),
        Ok(HostFileOutcome::Failed { code, message }) => host_file_failure(&code, &message),
        Ok(_) => api_error(
            StatusCode::BAD_GATEWAY,
            "host_unavailable",
            "the host answered a move request with an unexpected result",
        ),
    }
}

/// `files.remove`
pub async fn files_remove(
    State(state): State<AppState>,
    Json(request): Json<PathRequest>,
) -> Response {
    if request.path.trim().is_empty() {
        return invalid_path();
    }
    let scope = match file_scope(
        &state,
        request.host_id.as_deref(),
        request.root_path.as_deref(),
    ) {
        Ok(scope) => scope,
        Err(response) => return response,
    };
    let root = match root_required(&scope) {
        Ok(root) => root.to_owned(),
        Err(response) => return response,
    };
    let path = match resolve_path(&request.path, Some(&root)) {
        Ok(path) => path,
        Err(response) => return response,
    };
    let outcome = state
        .request_host_file(
            &scope.host_id,
            HostFileOperation::Remove {
                path,
                root_path: Some(root),
                recursive: request.recursive.unwrap_or(false),
            },
        )
        .await;
    match outcome {
        Err(error) => transport_error(error),
        Ok(HostFileOutcome::Done) => Json(json!({ "ok": true })).into_response(),
        Ok(HostFileOutcome::Failed { code, message }) => host_file_failure(&code, &message),
        Ok(_) => api_error(
            StatusCode::BAD_GATEWAY,
            "host_unavailable",
            "the host answered a remove request with an unexpected result",
        ),
    }
}

/// `files.read`: one file's bytes plus the hash a writer can round-trip.
pub async fn files_read(
    State(state): State<AppState>,
    Json(request): Json<ReadRequest>,
) -> Response {
    if request.path.trim().is_empty() {
        return invalid_path();
    }
    let scope = match file_scope(
        &state,
        request.host_id.as_deref(),
        request.root_path.as_deref(),
    ) {
        Ok(scope) => scope,
        Err(response) => return response,
    };
    let path = match resolve_path(&request.path, scope.root.as_deref()) {
        Ok(path) => path,
        Err(response) => return response,
    };
    let outcome = state
        .request_host_file(
            &scope.host_id,
            HostFileOperation::ReadWithMetadata {
                path,
                root_path: scope.root.clone(),
                max_bytes: MAX_FILE_OPERATION_BYTES,
            },
        )
        .await;
    match outcome {
        Err(error) => transport_error(error),
        Ok(HostFileOutcome::FileMetadata {
            content,
            content_encoding,
            size_bytes,
            sha256,
            modified_at_ms,
            ..
        }) => Json(json!({
            "path": request.path,
            "content": content,
            "contentEncoding": encoding_name(content_encoding),
            "sizeBytes": size_bytes,
            "sha256": sha256,
            "mimeType": Value::Null,
            "modifiedAtMs": modified_at_ms,
        }))
        .into_response(),
        Ok(HostFileOutcome::Failed { code, message }) => host_file_failure(&code, &message),
        Ok(_) => api_error(
            StatusCode::BAD_GATEWAY,
            "host_unavailable",
            "the host answered a read request with an unexpected result",
        ),
    }
}

/// `files.write`: replace a file, optionally under an optimistic-concurrency
/// check.
pub async fn files_write(
    State(state): State<AppState>,
    Json(request): Json<WriteRequest>,
) -> Response {
    if request.path.trim().is_empty() {
        return invalid_path();
    }
    let scope = match file_scope(
        &state,
        request.host_id.as_deref(),
        request.root_path.as_deref(),
    ) {
        Ok(scope) => scope,
        Err(response) => return response,
    };
    let root = match root_required(&scope) {
        Ok(root) => root.to_owned(),
        Err(response) => return response,
    };
    let path = match resolve_path(&request.path, Some(&root)) {
        Ok(path) => path,
        Err(response) => return response,
    };
    let encoding = match request.content_encoding.as_deref() {
        None | Some("utf8") => HostFileEncoding::Utf8,
        Some("base64") => HostFileEncoding::Base64,
        Some(other) => {
            return api_error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                format!("unknown contentEncoding {other:?}"),
            )
        }
    };
    if let Some(mode) = request.mode {
        if mode > 0o777 {
            return api_error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "mode must be at most 0o777",
            );
        }
    }
    // `expectedSha256` is tri-state in the contract: absent means "no check",
    // `null` means "the file must not exist yet", and a string means "the file
    // must hash to this". Conflating `null` with absent would turn a
    // create-only write into an unconditional overwrite.
    let (expected_sha256, create_only) = match &request.expected_sha256 {
        None => (None, false),
        Some(None) => (None, true),
        Some(Some(expected)) => (Some(expected.clone()), false),
    };
    let outcome = state
        .request_host_file(
            &scope.host_id,
            HostFileOperation::WriteFile {
                path,
                root_path: root,
                content: request.content,
                content_encoding: encoding,
                max_bytes: MAX_WRITE_BYTES,
                create_parents: request.create_parents.unwrap_or(false),
                expected_sha256,
                create_only,
                mode: request.mode,
            },
        )
        .await;
    match outcome {
        Err(error) => transport_error(error),
        Ok(HostFileOutcome::Written(written)) => {
            // The daemon hashes the bytes it wrote, so the contract's
            // `sha256` is reported from what is actually on disk rather than
            // recomputed here from a request body the daemon already bounded.
            let Some(sha256) = written.sha256.clone() else {
                return api_error(
                    StatusCode::BAD_GATEWAY,
                    "host_unavailable",
                    "the host wrote the file but did not report its hash",
                );
            };
            Json(json!({
                "outcome": "written",
                "sha256": sha256,
                "sizeBytes": written.size_bytes,
            }))
            .into_response()
        }
        Ok(HostFileOutcome::Conflict { current_sha256 }) => Json(json!({
            "outcome": "conflict",
            "currentSha256": current_sha256,
        }))
        .into_response(),
        Ok(HostFileOutcome::Failed { code, message }) => host_file_failure(&code, &message),
        Ok(_) => api_error(
            StatusCode::BAD_GATEWAY,
            "host_unavailable",
            "the host answered a write request with an unexpected result",
        ),
    }
}

fn encoding_name(encoding: HostFileEncoding) -> &'static str {
    match encoding {
        HostFileEncoding::Utf8 => "utf8",
        HostFileEncoding::Base64 => "base64",
    }
}

/* ------------------------------------------------------------------ */
/* Lifecycle cleanup                                                   */
/* ------------------------------------------------------------------ */

/// Closes every terminal a lifecycle change orphaned, on the hosts that own
/// them.
///
/// A thread or environment going away leaves terminals whose output no client
/// can ever render again. The records are settled here and the machines are
/// asked to kill the processes; the requests are best-effort because the
/// lifecycle change must succeed even if a host is unreachable — an orphaned
/// process on a disconnected machine is a leak, but refusing to delete the
/// thread would be a much worse one.
///
/// Spawned rather than awaited so a slow host cannot hold up a delete: the
/// client's `DELETE` should not wait on a relay round trip per session.
pub fn spawn_close_terminals(state: &AppState, ids: Vec<String>) {
    if ids.is_empty() {
        return;
    }
    let state = state.clone();
    tokio::spawn(async move {
        for id in ids {
            let Some(session) = state.terminals.get(&id) else {
                continue;
            };
            let _ = state
                .request_terminal(
                    &session.host_id,
                    TerminalOperation::Close {
                        id: id.clone(),
                        force: true,
                    },
                )
                .await;
        }
    });
}

/// Settles and kills the terminals a thread's teardown orphaned.
pub fn close_thread_terminals(
    state: &AppState,
    thread_id: &loom_domain::ThreadId,
    reason: loom_provider_protocol::TerminalCloseReason,
) {
    let closing = state
        .terminals
        .threads_to_close(thread_id, reason, loom_relay::now_ms());
    spawn_close_terminals(state, closing);
}

/// Settles and kills the terminals an environment's teardown orphaned.
pub fn close_environment_terminals(state: &AppState, environment_id: &loom_domain::EnvironmentId) {
    let closing = state.terminals.environments_to_close(
        environment_id,
        loom_provider_protocol::TerminalCloseReason::EnvironmentDestroyed,
        loom_relay::now_ms(),
    );
    spawn_close_terminals(state, closing);
}

/* ------------------------------------------------------------------ */
/* terminals.* routes                                                  */
/* ------------------------------------------------------------------ */

/// Resolves the host and initial directory for a terminal target.
struct TerminalPlan {
    host_id: HostId,
    cwd: String,
    target: TerminalTarget,
    thread_id: Option<loom_domain::ThreadId>,
    environment_id: Option<loom_domain::EnvironmentId>,
}

#[allow(clippy::result_large_err)]
fn plan_terminal_target(
    state: &AppState,
    target: &TargetRequest,
) -> Result<TerminalPlan, Response> {
    match target {
        TargetRequest::Thread { thread_id } => {
            let id = thread_id
                .parse::<loom_domain::ThreadId>()
                .map_err(|error| {
                    api_error(
                        StatusCode::BAD_REQUEST,
                        "invalid_request",
                        error.to_string(),
                    )
                })?;
            let Some(thread) = state.registry.thread(&id) else {
                return Err(api_error(
                    StatusCode::NOT_FOUND,
                    "thread_not_found",
                    format!("thread {id} is not known"),
                ));
            };
            let environment = thread_environment(state, &thread)?;
            let Some(cwd) = environment
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
            Ok(TerminalPlan {
                host_id: environment.host_id.clone(),
                cwd,
                target: TerminalTarget::Thread {
                    thread_id: id.clone(),
                },
                thread_id: Some(id),
                environment_id: Some(environment.id),
            })
        }
        TargetRequest::Environment { environment_id } => {
            let id = environment_id
                .parse::<loom_domain::EnvironmentId>()
                .map_err(|error| {
                    api_error(
                        StatusCode::BAD_REQUEST,
                        "invalid_request",
                        error.to_string(),
                    )
                })?;
            let Some(environment) = state.registry.environment(&id) else {
                return Err(api_error(
                    StatusCode::NOT_FOUND,
                    "environment_not_found",
                    format!("environment {id} is not known"),
                ));
            };
            let Some(cwd) = environment
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
            Ok(TerminalPlan {
                host_id: environment.host_id.clone(),
                cwd,
                target: TerminalTarget::Environment {
                    environment_id: id.clone(),
                },
                thread_id: None,
                environment_id: Some(id),
            })
        }
        TargetRequest::HostPath { host_id, cwd } => {
            let id = host_id.parse::<HostId>().map_err(|error| {
                api_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request",
                    error.to_string(),
                )
            })?;
            let host = state.registry.host(&id).ok_or_else(|| {
                api_error(
                    StatusCode::NOT_FOUND,
                    "host_not_found",
                    format!("host {id} is not known"),
                )
            })?;
            // A `host_path` target names a host and, optionally, where to
            // start. With no cwd the host's own reported directory is used,
            // because the control plane cannot invent a path on a machine it
            // does not own.
            let cwd = match cwd.clone().filter(|path| !path.trim().is_empty()) {
                Some(cwd) => cwd,
                None => host.data_dir.clone().ok_or_else(|| {
                    api_error(
                        StatusCode::NOT_IMPLEMENTED,
                        "not_configured",
                        format!("host {id} has not reported a default directory"),
                    )
                })?,
            };
            Ok(TerminalPlan {
                host_id: id.clone(),
                target: TerminalTarget::HostPath {
                    host_id: id,
                    cwd: Some(cwd.clone()),
                },
                cwd,
                thread_id: None,
                environment_id: None,
            })
        }
    }
}

/// The environment a thread's terminal runs in.
#[allow(clippy::result_large_err)]
fn thread_environment(state: &AppState, thread: &Thread) -> Result<Environment, Response> {
    let Some(environment_id) = thread.environment_id.clone() else {
        return Err(api_error(
            StatusCode::CONFLICT,
            "thread_environment_unavailable",
            "thread has no environment; bind one before opening a terminal",
        ));
    };
    state.registry.environment(&environment_id).ok_or_else(|| {
        api_error(
            StatusCode::CONFLICT,
            "thread_environment_unavailable",
            format!("environment {environment_id} is not known"),
        )
    })
}

/// `terminals.list`
pub async fn terminals_list(
    State(state): State<AppState>,
    Query(query): Query<TerminalListQuery>,
) -> Response {
    let mut sessions = state.terminals.list();
    if let Some(host_id) = query.host_id.as_deref().filter(|raw| !raw.is_empty()) {
        sessions.retain(|session| session.host_id.to_string() == host_id);
    }
    if let Some(thread_id) = query.thread_id.as_deref().filter(|raw| !raw.is_empty()) {
        sessions.retain(|session| {
            session
                .thread_id
                .as_ref()
                .is_some_and(|id| id.to_string() == thread_id)
        });
    }
    if let Some(environment_id) = query
        .environment_id
        .as_deref()
        .filter(|raw| !raw.is_empty())
    {
        sessions.retain(|session| {
            session
                .environment_id
                .as_ref()
                .is_some_and(|id| id.to_string() == environment_id)
        });
    }
    if let Some(cwd) = query.cwd.as_deref().filter(|raw| !raw.is_empty()) {
        sessions.retain(|session| session.initial_cwd == cwd);
    }
    Json(json!({
        "sessions": sessions.iter().map(session_value).collect::<Vec<_>>(),
    }))
    .into_response()
}

/// `terminals.get`
pub async fn terminals_get(
    State(state): State<AppState>,
    AxumPath(terminal_id): AxumPath<String>,
) -> Response {
    match state.terminals.get(&terminal_id) {
        Some(session) => Json(session_value(&session)).into_response(),
        None => api_error(
            StatusCode::NOT_FOUND,
            "terminal_not_found",
            format!("terminal {terminal_id} is not known"),
        ),
    }
}

/// `terminals.create`
pub async fn terminals_create(
    State(state): State<AppState>,
    Json(request): Json<CreateTerminalRequest>,
) -> Response {
    if request.cols == 0
        || request.cols > MAX_TERMINAL_COLS
        || request.rows == 0
        || request.rows > MAX_TERMINAL_ROWS
    {
        return api_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            format!("cols must be 1-{MAX_TERMINAL_COLS} and rows 1-{MAX_TERMINAL_ROWS}"),
        );
    }
    if let Some(StartRequest::Command { command }) = &request.start {
        if command.is_empty() || command.len() > 10_000 {
            return api_error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "command must be 1-10000 characters",
            );
        }
    }
    let plan = match plan_terminal_target(&state, &request.target) {
        Ok(plan) => plan,
        Err(response) => return response,
    };
    let start = match request.start {
        Some(StartRequest::Command { command }) => TerminalStart::Command { command },
        Some(StartRequest::Shell) | None => TerminalStart::Shell,
    };
    let title = request
        .title
        .clone()
        .filter(|title| !title.trim().is_empty())
        .unwrap_or_else(|| default_title(&start, &plan.cwd));
    let now = loom_relay::now_ms();
    let id = TerminalSessions::mint_id();
    let outcome = state
        .request_terminal(
            &plan.host_id,
            TerminalOperation::Create {
                id: id.clone(),
                start,
                target: plan.target.clone(),
                cols: request.cols,
                rows: request.rows,
                title,
                cwd: plan.cwd.clone(),
            },
        )
        .await;
    match outcome {
        Err(error) => terminal_transport_error(error),
        Ok(TerminalOutcome::Session { session }) => {
            let session = adopt(session, &plan, now);
            state.terminals.put(session.clone());
            (StatusCode::CREATED, Json(session_value(&session))).into_response()
        }
        Ok(TerminalOutcome::Failed { code, message }) => terminal_failure(&code, &message),
        Ok(_) => api_error(
            StatusCode::BAD_GATEWAY,
            "host_unavailable",
            "the host answered a create request with an unexpected result",
        ),
    }
}

/// Normalises the daemon's session into the record the control plane stores.
///
/// The daemon owns the process and reports the resolved cwd; the server trusts
/// those, but re-stamps the ownership fields from the plan it resolved so the
/// two can never disagree about which thread or environment a session belongs
/// to.
fn adopt(mut session: TerminalSession, plan: &TerminalPlan, now_ms: u64) -> TerminalSession {
    session.id = if session.id.is_empty() {
        TerminalSessions::mint_id()
    } else {
        session.id
    };
    session.thread_id = plan.thread_id.clone();
    session.environment_id = plan.environment_id.clone();
    session.host_id = plan.host_id.clone();
    if session.initial_cwd.is_empty() {
        session.initial_cwd = plan.cwd.clone();
    }
    if session.created_at_ms == 0 {
        session.created_at_ms = now_ms;
    }
    if session.updated_at_ms == 0 {
        session.updated_at_ms = now_ms;
    }
    if session.title.is_empty() {
        session.title = default_title(&TerminalStart::Shell, &session.initial_cwd);
    }
    session
}

fn default_title(_start: &TerminalStart, cwd: &str) -> String {
    let name = cwd
        .trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
        .filter(|name| !name.is_empty())
        .unwrap_or("terminal");
    name.to_owned()
}

/// `terminals.input`
pub async fn terminals_input(
    State(state): State<AppState>,
    AxumPath(terminal_id): AxumPath<String>,
    Json(request): Json<TerminalInputRequest>,
) -> Response {
    let Some(session) = state.terminals.get(&terminal_id) else {
        return api_error(
            StatusCode::NOT_FOUND,
            "terminal_not_found",
            format!("terminal {terminal_id} is not known"),
        );
    };
    // Empty input is refused before a round trip: the contract declares a
    // one-character minimum, and a no-op write cannot be distinguished from a
    // dropped one by the caller.
    if request.data_base64.is_empty() {
        return api_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "dataBase64 must not be empty",
        );
    }
    let outcome = state
        .request_terminal(
            &session.host_id,
            TerminalOperation::Input {
                id: terminal_id.clone(),
                data_base64: request.data_base64,
            },
        )
        .await;
    terminal_session_outcome(&state, terminal_id, outcome, StatusCode::OK)
}

/// `terminals.resize`
pub async fn terminals_resize(
    State(state): State<AppState>,
    AxumPath(terminal_id): AxumPath<String>,
    Json(request): Json<TerminalResizeRequest>,
) -> Response {
    let Some(session) = state.terminals.get(&terminal_id) else {
        return api_error(
            StatusCode::NOT_FOUND,
            "terminal_not_found",
            format!("terminal {terminal_id} is not known"),
        );
    };
    if request.cols == 0
        || request.cols > MAX_TERMINAL_COLS
        || request.rows == 0
        || request.rows > MAX_TERMINAL_ROWS
    {
        return api_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            format!("cols must be 1-{MAX_TERMINAL_COLS} and rows 1-{MAX_TERMINAL_ROWS}"),
        );
    }
    let outcome = state
        .request_terminal(
            &session.host_id,
            TerminalOperation::Resize {
                id: terminal_id.clone(),
                cols: request.cols,
                rows: request.rows,
            },
        )
        .await;
    terminal_session_outcome(&state, terminal_id, outcome, StatusCode::OK)
}

/// `terminals.close`
pub async fn terminals_close(
    State(state): State<AppState>,
    AxumPath(terminal_id): AxumPath<String>,
    Json(request): Json<TerminalCloseRequest>,
) -> Response {
    let Some(session) = state.terminals.get(&terminal_id) else {
        return api_error(
            StatusCode::NOT_FOUND,
            "terminal_not_found",
            format!("terminal {terminal_id} is not known"),
        );
    };
    if !matches!(request.mode.as_str(), "force" | "if-clean") {
        return api_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            format!("unknown close mode {:?}", request.mode),
        );
    }
    if request.reason != "user" {
        return api_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "the contract only accepts reason `user`",
        );
    }
    let outcome = state
        .request_terminal(
            &session.host_id,
            TerminalOperation::Close {
                id: terminal_id.clone(),
                force: request.mode == "force",
            },
        )
        .await;
    terminal_session_outcome(&state, terminal_id, outcome, StatusCode::OK)
}

/// `terminals.restart`
pub async fn terminals_restart(
    State(state): State<AppState>,
    AxumPath(terminal_id): AxumPath<String>,
) -> Response {
    let Some(session) = state.terminals.get(&terminal_id) else {
        return api_error(
            StatusCode::NOT_FOUND,
            "terminal_not_found",
            format!("terminal {terminal_id} is not known"),
        );
    };
    let outcome = state
        .request_terminal(
            &session.host_id,
            TerminalOperation::Restart {
                id: terminal_id.clone(),
            },
        )
        .await;
    match outcome {
        Err(error) => terminal_transport_error(error),
        Ok(TerminalOutcome::Session { session }) => {
            state.terminals.put(session.clone());
            (StatusCode::CREATED, Json(session_value(&session))).into_response()
        }
        Ok(TerminalOutcome::Failed { code, message }) => terminal_failure(&code, &message),
        Ok(_) => api_error(
            StatusCode::BAD_GATEWAY,
            "host_unavailable",
            "the host answered a restart request with an unexpected result",
        ),
    }
}

/// `terminals.update`: rename a session.
///
/// A title is control-plane metadata: it changes nothing about the process, so
/// this never touches the host. That also means a disconnected session can
/// still be renamed, which is the honest behaviour — the label a user chose is
/// not a property of a machine.
pub async fn terminals_update(
    State(state): State<AppState>,
    AxumPath(terminal_id): AxumPath<String>,
    Json(request): Json<TerminalUpdateRequest>,
) -> Response {
    if request.title.trim().is_empty() || request.title.len() > 200 {
        return api_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "title must be 1-200 characters",
        );
    }
    let Some(mut session) = state.terminals.get(&terminal_id) else {
        return api_error(
            StatusCode::NOT_FOUND,
            "terminal_not_found",
            format!("terminal {terminal_id} is not known"),
        );
    };
    session.title = request.title;
    session.updated_at_ms = loom_relay::now_ms();
    state.terminals.put(session.clone());
    Json(session_value(&session)).into_response()
}

/// `terminals.output`
pub async fn terminals_output(
    State(state): State<AppState>,
    AxumPath(terminal_id): AxumPath<String>,
    Query(query): Query<TerminalOutputQuery>,
) -> Response {
    let Some(session) = state.terminals.get(&terminal_id) else {
        return api_error(
            StatusCode::NOT_FOUND,
            "terminal_not_found",
            format!("terminal {terminal_id} is not known"),
        );
    };
    let since_seq = match parse_u64(query.since_seq.as_deref(), "sinceSeq") {
        Ok(value) => value.unwrap_or(0),
        Err(response) => return response,
    };
    let limit = match parse_u64(query.limit_chunks.as_deref(), "limitChunks") {
        Ok(value) => value
            .map(|value| (value as usize).clamp(1, MAX_OUTPUT_CHUNKS))
            .unwrap_or(DEFAULT_OUTPUT_CHUNKS),
        Err(response) => return response,
    };
    let tail_bytes = match parse_u64(query.tail_bytes.as_deref(), "tailBytes") {
        Ok(value) => value
            .map(|value| value.clamp(1, MAX_TAIL_BYTES))
            .unwrap_or(DEFAULT_TAIL_BYTES),
        Err(response) => return response,
    };
    let outcome = state
        .request_terminal(
            &session.host_id,
            TerminalOperation::Output {
                id: terminal_id.clone(),
                since_seq,
                limit,
                tail_bytes,
            },
        )
        .await;
    match outcome {
        Err(error) => terminal_transport_error(error),
        Ok(TerminalOutcome::Output {
            chunks,
            next_seq,
            truncated,
        }) => Json(json!({
            "chunks": chunks
                .iter()
                .map(|chunk| json!({ "seq": chunk.seq, "dataBase64": chunk.data_base64 }))
                .collect::<Vec<_>>(),
            "nextSeq": next_seq,
            "truncated": truncated,
        }))
        .into_response(),
        Ok(TerminalOutcome::Failed { code, message }) => terminal_failure(&code, &message),
        Ok(_) => api_error(
            StatusCode::BAD_GATEWAY,
            "host_unavailable",
            "the host answered an output request with an unexpected result",
        ),
    }
}

#[allow(clippy::result_large_err)]
fn parse_u64(raw: Option<&str>, field: &str) -> Result<Option<u64>, Response> {
    match raw.filter(|raw| !raw.is_empty()) {
        None => Ok(None),
        Some(raw) => raw.parse::<u64>().map(Some).map_err(|_| {
            api_error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                format!("{field} must be a non-negative integer"),
            )
        }),
    }
}

/// Converts a session-returning terminal outcome and stores the update.
fn terminal_session_outcome(
    state: &AppState,
    terminal_id: String,
    outcome: Result<TerminalOutcome, TerminalTransportError>,
    success: StatusCode,
) -> Response {
    match outcome {
        Err(error) => terminal_transport_error(error),
        Ok(TerminalOutcome::Session { session }) => {
            if session.id.is_empty() {
                // A daemon that omitted the id still answered about the session
                // it was asked about; adopting the requested id keeps the
                // control-plane index keyed the way the client addresses it.
                let mut session = session;
                session.id = terminal_id;
                state.terminals.put(session.clone());
                return (success, Json(session_value(&session))).into_response();
            }
            state.terminals.put(session.clone());
            (success, Json(session_value(&session))).into_response()
        }
        Ok(TerminalOutcome::Failed { code, message }) => terminal_failure(&code, &message),
        Ok(_) => api_error(
            StatusCode::BAD_GATEWAY,
            "host_unavailable",
            "the host answered a session request with an unexpected result",
        ),
    }
}

/// Projects a session into the contract's `terminalSessionSchema`.
///
/// Every required field is present, with `null` where the session has no value:
/// the schema declares `threadId`, `environmentId`, `exitCode` and
/// `closeReason` as nullable but required, and omitting one would fail
/// validation.
fn session_value(session: &TerminalSession) -> Value {
    json!({
        "id": session.id,
        "threadId": session.thread_id.as_ref().map(ToString::to_string),
        "environmentId": session.environment_id.as_ref().map(ToString::to_string),
        "hostId": session.host_id.to_string(),
        "title": session.title,
        "initialCwd": session.initial_cwd,
        "cols": session.cols,
        "rows": session.rows,
        "status": match session.status {
            TerminalStatus::Starting => "starting",
            TerminalStatus::Running => "running",
            TerminalStatus::Disconnected => "disconnected",
            TerminalStatus::Exited => "exited",
        },
        "exitCode": session.exit_code,
        "closeReason": session.close_reason.map(|reason| match reason {
            loom_provider_protocol::TerminalCloseReason::User => "user",
            loom_provider_protocol::TerminalCloseReason::ThreadDeleted => "thread-deleted",
            loom_provider_protocol::TerminalCloseReason::ProcessExit => "process-exit",
            loom_provider_protocol::TerminalCloseReason::DaemonDisconnect => "daemon-disconnect",
            loom_provider_protocol::TerminalCloseReason::EnvironmentDestroyed => "environment-destroyed",
            loom_provider_protocol::TerminalCloseReason::ThreadArchived => "thread-archived",
            loom_provider_protocol::TerminalCloseReason::OpenTimeout => "open-timeout",
        }),
        "createdAt": session.created_at_ms,
        "updatedAt": session.updated_at_ms,
        "lastUserInputAt": session.last_user_input_at_ms,
    })
}
