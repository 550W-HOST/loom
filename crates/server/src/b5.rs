//! Batch B5: thread files, pane actions and thread storage.
//!
//! Ten bb routes whose whole point is that a thread's files live on a **host**.
//! The control plane has no business reading its own disk and calling the
//! result a thread's file, so each of these routes resolves the thread's
//! environment, names the host that owns it, and asks that host through
//! [`crate::host_files`]. A thread with no environment, or an environment that
//! is not `ready`, is refused with the contract's own lifecycle error rather
//! than answered from whatever happens to be on the server's filesystem.
//!
//! # The routes
//!
//! | route | what it answers |
//! | --- | --- |
//! | `threads.count` | how many threads match a filter, optionally grouped |
//! | `threads.paneAction` | a pane signal fanned out to the thread's room |
//! | `threads.rawFile` | an HTML preview read from an absolute host path |
//! | `threads.hostFileContent` | an absolute host path's bytes |
//! | `threads.worktreeFile` | a root-relative path inside the workspace |
//! | `threads.storageContent` | a root-relative path inside thread storage |
//! | `threads.storageFile` | the same, with the path in the URL |
//! | `threads.storageFiles` | thread storage's files |
//! | `threads.storagePaths` | thread storage's files and/or directories |
//! | `threads.storageLocation` | which host owns the storage, and where it is |
//!
//! # Permission boundaries
//!
//! Three distinct scopes, and they are not interchangeable:
//!
//! * **Workspace scope** (`worktreeFile`) — a root-relative path, resolved
//!   against the environment's own workspace. The host re-checks that the
//!   *resolved* path is inside the workspace, so a symlink cannot leave it.
//! * **Storage scope** (`storageContent`, `storageFile`, `storageFiles`,
//!   `storagePaths`, `storageLocation`) — a root-relative path inside the
//!   thread's storage directory, whose root comes from the data directory the
//!   host reported. Same containment check.
//! * **Absolute host scope** (`hostFileContent`, `rawFile`) — an absolute path
//!   the client names. This one is deliberately *not* root-confined, because
//!   the client is pointing at a file it already knows the location of (an
//!   image in a timeline, a log a tool wrote). It is still confined to the
//!   thread's own host: the path is read on the machine that owns the thread's
//!   environment, never on the server.
//!
//! Every relative path is validated before a request is built: NUL, a leading
//! `/`, and any `.`/`..` segment are refused with `400 invalid_path`, exactly
//! as bb's `parseSafeRelativeRoutePath` does. The host re-checks containment on
//! the resolved path, which is the half the control plane cannot do.

// Every handler here answers with `Response`, which is large enough that
// clippy's `result_large_err` fires on each helper returning one. Boxing each
// error would cost an allocation on the hot failure path and obscure the
// handlers; the existing HTTP module carries the same allow for the same
// reason.
#![allow(clippy::result_large_err)]

use std::path::Path;

use axum::body::Body;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use loom_domain::{Environment, Thread, ThreadId, ThreadStatus};
use loom_provider_protocol::{
    thread_storage_root, HostFileContent, HostFileEncoding, HostFileEntry, HostFileOperation,
    HostFileOutcome,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::host_files::HostFileTransportError;
use crate::state::AppState;

/// The largest file any content route will serve.
///
/// bb sets 25 MB for non-image files and 10 MB for images; one bound is used
/// here because the worker is the side that actually enforces it from the
/// request, and a single number cannot drift between the two.
pub const MAX_FILE_CONTENT_BYTES: u64 = 25 * 1024 * 1024;

/// bb's HTML preview cap, which is tighter than the general one.
pub const MAX_HTML_PREVIEW_BYTES: u64 = 5 * 1024 * 1024;

/// Default and maximum entries in a listing, from bb's `FILE_LIST_LIMIT_MAX`.
pub const FILE_LIST_LIMIT_DEFAULT: usize = 1000;
/// The ceiling a client's `limit` is clamped to.
pub const FILE_LIST_LIMIT_MAX: usize = 10_000;

/// The sentinel `parentThreadId` value meaning "root threads only".
///
/// A thread id can never be this — ids are prefixed `thr_` — so the value is
/// unambiguous, which is why bb chose it over an empty string.
pub const THREAD_COUNT_ROOT_PARENT: &str = "none";

/* ------------------------------------------------------------------ */
/* Path validation                                                     */
/* ------------------------------------------------------------------ */

/// Why a client-supplied path was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathError {
    /// The path is empty, absolute, or contains a `.`/`..` segment, a NUL or a
    /// backslash.
    Invalid,
}

/// Validates a **root-relative** path, mirroring bb's
/// `parseSafeRelativeRoutePath`.
///
/// `\` is refused rather than normalised: bb refuses it, and accepting it would
/// mean a Windows client and a POSIX host could disagree about whether
/// `a\..\b` leaves the root.
pub fn validate_relative_path(raw: &str) -> Result<&str, PathError> {
    if raw.is_empty()
        || raw.contains('\0')
        || raw.contains('\\')
        || Path::new(raw).is_absolute()
        || raw.starts_with('/')
    {
        return Err(PathError::Invalid);
    }
    if raw
        .split('/')
        .any(|segment| segment.is_empty() || segment == "." || segment == "..")
    {
        return Err(PathError::Invalid);
    }
    Ok(raw)
}

/// Validates an **absolute host** path, mirroring bb's
/// `parseRawFilesystemPath`.
///
/// An absolute path is the point of these routes, so this only refuses a NUL —
/// the payload that would truncate a path in a C API — and a relative value,
/// which cannot be a file the client meant. Both POSIX and Windows spellings are
/// accepted, because the host may be a Windows machine even though this server
/// is not.
pub fn validate_absolute_path(raw: &str) -> Result<&str, PathError> {
    if raw.is_empty() || raw.contains('\0') {
        return Err(PathError::Invalid);
    }
    let bytes = raw.as_bytes();
    let windows_absolute = bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'/' || bytes[2] == b'\\');
    if !raw.starts_with('/') && !windows_absolute {
        return Err(PathError::Invalid);
    }
    Ok(raw)
}

/// Joins a root and a validated relative path with `/` separators.
///
/// String concatenation rather than `Path::join` on purpose: the root is a path
/// string reported by another machine, and the result crosses back to that
/// machine as a string. Normalising it here would apply *this* platform's path
/// rules to *that* platform's filesystem.
pub fn join_root(root: &str, relative: &str) -> String {
    format!("{}/{}", root.trim_end_matches(['/', '\\']), relative)
}

fn invalid_path_response() -> Response {
    api_error(StatusCode::BAD_REQUEST, "invalid_path", "Invalid file path")
}

fn api_error(status: StatusCode, code: &'static str, message: impl Into<String>) -> Response {
    (
        status,
        Json(json!({ "code": code, "message": message.into() })),
    )
        .into_response()
}

/// Maps a transport failure onto the contract's error vocabulary.
fn transport_error_response(error: HostFileTransportError) -> Response {
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

/// Maps a host-reported failure onto the same vocabulary.
fn host_failure_response(code: &str, message: &str) -> Response {
    let status = match code {
        "not_found" => StatusCode::NOT_FOUND,
        "invalid_path" => StatusCode::BAD_REQUEST,
        "file_too_large" => StatusCode::PAYLOAD_TOO_LARGE,
        "unsupported_media_type" => StatusCode::UNSUPPORTED_MEDIA_TYPE,
        _ => StatusCode::BAD_GATEWAY,
    };
    // The worker's own code crosses over when the contract knows it; anything
    // else becomes the contract's generic host failure at the same status,
    // rather than inventing an error code no client can branch on.
    let code: &'static str = match code {
        "not_found" => "not_found",
        "invalid_path" => "invalid_path",
        "file_too_large" => "file_too_large",
        "unsupported_media_type" => "unsupported_media_type",
        _ => "host_unavailable",
    };
    api_error(status, code, message)
}

/* ------------------------------------------------------------------ */
/* Thread → host resolution                                            */
/* ------------------------------------------------------------------ */

/// The environment a content route needs: `ready`, with a workspace path.
fn ready_environment(state: &AppState, thread: &Thread) -> Result<Environment, Response> {
    let Some(environment_id) = thread.environment_id.clone() else {
        return Err(api_error(
            StatusCode::CONFLICT,
            "thread_environment_unavailable",
            "thread has no environment; bind one before reading its files",
        ));
    };
    let Some(environment) = state.registry.environment(&environment_id) else {
        return Err(api_error(
            StatusCode::CONFLICT,
            "thread_environment_unavailable",
            format!("environment {environment_id} is not known"),
        ));
    };
    Ok(environment)
}

/// The environment's workspace path, for a route that reads inside it.
fn workspace_root(state: &AppState, thread: &Thread) -> Result<(Environment, String), Response> {
    let environment = ready_environment(state, thread)?;
    // A workspace path is required even for an `error` environment: the
    // directory exists, the provider just could not use it. Reading a file out
    // of it is exactly how a client shows why the turn failed.
    let Some(path) = environment.path.clone() else {
        return Err(api_error(
            StatusCode::CONFLICT,
            "environment_not_ready",
            format!("environment {} has no workspace path", environment.id),
        ));
    };
    Ok((environment, path))
}

/// The host and storage root for a thread, from the layout the host reported.
///
/// A host that never reported a data directory cannot have its storage named,
/// and the control plane refuses rather than guessing a path on a machine it
/// does not own.
fn storage_target(
    state: &AppState,
    thread: &Thread,
) -> Result<(loom_domain::HostId, String), Response> {
    let environment = ready_environment(state, thread)?;
    let host_id = environment.host_id.clone();
    let Some(host) = state.registry.host(&host_id) else {
        return Err(api_error(
            StatusCode::CONFLICT,
            "host_unavailable",
            format!("host {host_id} is not enrolled on this server"),
        ));
    };
    let Some(data_dir) = host.data_dir.clone() else {
        return Err(api_error(
            StatusCode::NOT_IMPLEMENTED,
            "not_configured",
            format!(
                "host {} has not reported a data directory, so thread storage cannot be located",
                host.id
            ),
        ));
    };
    let root = thread_storage_root(&data_dir, &thread.id.to_string());
    Ok((host_id, root))
}

/* ------------------------------------------------------------------ */
/* Query types                                                         */
/* ------------------------------------------------------------------ */

/// Query for the three absolute-path content routes.
#[derive(Clone, Debug, Deserialize)]
pub struct ContentPathQuery {
    /// Absolute path on the thread's host.
    path: String,
}

/// Query for a listing.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileListQuery {
    query: Option<String>,
    limit: Option<String>,
}

/// Query for a path listing, which additionally chooses the entry kinds.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PathListQuery {
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    limit: Option<String>,
    include_files: String,
    include_directories: String,
}

/// Query for the thread count.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadCountQuery {
    status: Option<String>,
    host_id: Option<String>,
    provider_id: Option<String>,
    project_id: Option<String>,
    parent_thread_id: Option<String>,
    group_by: Option<String>,
    include_archived: Option<String>,
    include_hidden: Option<String>,
}

/// Body of a pane action request.
#[derive(Clone, Debug, Deserialize)]
pub struct PaneActionRequest {
    action: String,
}

/// A parsed listing limit, clamped to bb's ceiling.
#[allow(clippy::result_large_err)]
fn parse_limit(raw: Option<&String>) -> Result<usize, Response> {
    match raw {
        None => Ok(FILE_LIST_LIMIT_DEFAULT),
        Some(raw) => {
            let parsed = raw.parse::<usize>().map_err(|_| {
                api_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request",
                    "limit must be a positive integer",
                )
            })?;
            if parsed == 0 {
                return Err(api_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request",
                    "limit must be a positive integer",
                ));
            }
            Ok(parsed.min(FILE_LIST_LIMIT_MAX))
        }
    }
}

/* ------------------------------------------------------------------ */
/* Response construction                                               */
/* ------------------------------------------------------------------ */

/// The content-type and disposition headers a raw file response carries.
///
/// HTML is served as `text/html` with a sandboxing CSP and `no-store`, because
/// it is rendered; anything else gets `nosniff` and the host's own media type.
///
/// # Why there is no conditional request
///
/// bb's daemon returns an entity tag derived from the file's SHA-256 and answers
/// `If-None-Match` with `304`. Loom's host protocol carries no content hash, and
/// synthesising a tag from the size and path would be wrong in the direction
/// that matters: a same-length edit in place would keep the old tag, and a
/// client would render stale bytes. Serving the file again is the cheap, correct
/// answer, so `If-None-Match` is ignored and every read is a `200` — a
/// deliberate divergence, recorded in `docs/contract.md`.
fn file_response(content: HostFileContent) -> Response {
    let bytes = match content.content_encoding {
        HostFileEncoding::Utf8 => content.content.into_bytes(),
        HostFileEncoding::Base64 => match base64_decode(&content.content) {
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
    let is_html = content
        .mime_type
        .as_deref()
        .is_some_and(|mime| mime.eq_ignore_ascii_case("text/html"));

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
    if is_html {
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        headers.insert(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static("sandbox allow-scripts"),
        );
    }
    (StatusCode::OK, headers, Body::from(bytes)).into_response()
}

/// A file's raw bytes, guarded by the HTML preview cap where it applies.
pub(crate) fn content_response(content: HostFileContent, relative_path: Option<&str>) -> Response {
    let is_html = relative_path
        .map(|path| path.to_ascii_lowercase().ends_with(".html"))
        .unwrap_or_else(|| {
            content
                .mime_type
                .as_deref()
                .is_some_and(|mime| mime.eq_ignore_ascii_case("text/html"))
        });
    if is_html && content.size_bytes > MAX_HTML_PREVIEW_BYTES {
        return api_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "file_too_large",
            "HTML preview exceeds the 5 MB limit",
        );
    }
    file_response(content)
}

/// The body of a content query, on the host's answer.
fn content_from_outcome(outcome: HostFileOutcome, relative: Option<&str>) -> Response {
    match outcome {
        HostFileOutcome::Content(content) => content_response(content, relative),
        HostFileOutcome::Listing { .. } => api_error(
            StatusCode::BAD_GATEWAY,
            "host_unavailable",
            "the host answered a content request with a listing",
        ),
        HostFileOutcome::Failed { code, message } => host_failure_response(&code, &message),
        // Write, copy and the newer path operations exist for other routes; a
        // read route can never legitimately receive one, so it is a host
        // protocol mistake rather than a result to reinterpret.
        HostFileOutcome::Written(_)
        | HostFileOutcome::Copied { .. }
        | HostFileOutcome::FileMetadata { .. }
        | HostFileOutcome::Conflict { .. }
        | HostFileOutcome::Done => api_error(
            StatusCode::BAD_GATEWAY,
            "host_unavailable",
            "the host answered a content request with a non-content result",
        ),
    }
}

/// The two entry shapes a listing response uses.
///
/// The contract distinguishes them and both are `additionalProperties: false`,
/// so the projection must too: `fileSchema` is `{path, name}`, while a path
/// entry additionally carries its kind and the query's score and match
/// positions.
fn entry_value(entry: &HostFileEntry, with_scores: bool) -> Value {
    let kind = match entry.kind {
        loom_provider_protocol::HostPathKind::File => "file",
        loom_provider_protocol::HostPathKind::Directory => "directory",
    };
    if with_scores {
        json!({
            "kind": kind,
            "path": entry.path,
            "name": entry.name,
            "score": entry.score,
            "positions": entry.positions,
        })
    } else {
        json!({ "path": entry.path, "name": entry.name })
    }
}

/* ------------------------------------------------------------------ */
/* Route handlers                                                      */
/* ------------------------------------------------------------------ */

/// `threads.count`: how many threads match a filter, optionally grouped.
///
/// A count over the entity view the server holds. It is deliberately computed
/// from the same rows `threads.list` would return, so the two cannot disagree
/// about which threads exist.
pub async fn thread_count(
    State(state): State<AppState>,
    Query(query): Query<ThreadCountQuery>,
) -> Response {
    let include_archived = query.include_archived.as_deref() == Some("true");
    let include_hidden = query.include_hidden.as_deref() == Some("true");
    let status_filter = match query.status.as_deref() {
        None => None,
        Some(raw) => match bb_status_to_thread_status(raw) {
            Some(status) => Some(status),
            None => {
                return api_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request",
                    format!("unknown thread status {raw:?}"),
                )
            }
        },
    };
    let parent_filter = thread_count_parent_filter(query.parent_thread_id.as_deref());
    let group_by = query.group_by.as_deref();
    if let Some(group_by) = group_by {
        if !matches!(group_by, "host" | "provider" | "project") {
            return api_error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                format!("unknown groupBy {group_by:?}"),
            );
        }
    }

    let provider_id = state.provider_spec().name.clone();
    let mut matching: Vec<Thread> = state
        .registry
        .threads()
        .into_iter()
        .filter(|thread| thread.deleted_at_ms.is_none())
        .filter(|thread| include_archived || thread.status != ThreadStatus::Archived)
        .filter(|thread| {
            include_hidden || thread.visibility == loom_domain::ThreadVisibility::Visible
        })
        .filter(|thread| status_filter.is_none_or(|status| thread.status == status))
        .filter(|thread| match &parent_filter {
            None => true,
            Some(ThreadParentFilter::Root) => thread.parent_thread_id.is_none(),
            Some(ThreadParentFilter::Id(id)) => thread.parent_thread_id.as_ref() == Some(id),
        })
        .filter(|thread| {
            query
                .project_id
                .as_deref()
                .is_none_or(|project_id| thread.project_id.to_string() == project_id)
        })
        // `providerId` is the configured provider for every thread: loom runs
        // one provider, so a thread cannot belong to a different one, and
        // reporting that honestly beats a per-thread field nothing sets.
        .filter(|_| {
            query
                .provider_id
                .as_deref()
                .is_none_or(|requested| requested == provider_id)
        })
        .filter(|thread| {
            query.host_id.as_deref().is_none_or(|host_id| {
                thread
                    .environment_id
                    .as_ref()
                    .and_then(|id| state.registry.environment(id))
                    .is_some_and(|environment| environment.host_id.to_string() == host_id)
            })
        })
        .collect();
    matching.sort_by(|left, right| left.id.cmp(&right.id));

    let total = matching.len();
    let mut body = json!({ "total": total });
    if let Some(group_by) = group_by {
        let mut groups: Vec<(Option<String>, usize)> = Vec::new();
        for thread in &matching {
            let key = match group_by {
                "host" => thread
                    .environment_id
                    .as_ref()
                    .and_then(|id| state.registry.environment(id))
                    .map(|environment| environment.host_id.to_string()),
                "provider" => Some(provider_id.clone()),
                // `project` is the only grouping whose key is always present: a
                // thread must name a project. The other two can be `null`, which
                // is what the contract's nullable key is for.
                _ => Some(thread.project_id.to_string()),
            };
            match groups.iter_mut().find(|(existing, _)| *existing == key) {
                Some((_, count)) => *count += 1,
                None => groups.push((key, 1)),
            }
        }
        groups.sort_by(|left, right| left.0.cmp(&right.0));
        body["groups"] = Value::Array(
            groups
                .into_iter()
                .map(|(key, count)| {
                    json!({
                        "key": key.map_or(Value::Null, Value::String),
                        "count": count,
                    })
                })
                .collect(),
        );
    }
    Json(body).into_response()
}

/// Which parentage a `threads.count` filter asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
enum ThreadParentFilter {
    Root,
    Id(ThreadId),
}

fn thread_count_parent_filter(raw: Option<&str>) -> Option<ThreadParentFilter> {
    let raw = raw?;
    if raw == THREAD_COUNT_ROOT_PARENT {
        return Some(ThreadParentFilter::Root);
    }
    raw.parse::<ThreadId>().ok().map(ThreadParentFilter::Id)
}

/// The bb status word a loom status is reported as.
///
/// The inverse of the projection `thread_summary_value` uses, so a filter the
/// client read off a thread row matches that row.
fn bb_status_to_thread_status(raw: &str) -> Option<ThreadStatus> {
    match raw {
        "idle" => Some(ThreadStatus::Idle),
        "active" => Some(ThreadStatus::Working),
        "error" => Some(ThreadStatus::Error),
        _ => None,
    }
}

/// `threads.paneAction`: fan a pane signal out to the thread's room.
///
/// Like `threads.open`, the request travels the only path the control plane
/// has — into the thread's relay room — and `delivered` counts the room's local
/// subscribers. It is idempotent for a client that sees it twice.
pub async fn thread_pane_action(
    State(state): State<AppState>,
    AxumPath(raw_thread_id): AxumPath<String>,
    Json(request): Json<PaneActionRequest>,
) -> Response {
    let Ok(thread_id) = raw_thread_id.parse::<ThreadId>() else {
        return api_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "invalid thread id",
        );
    };
    let Ok(thread) = public_thread(&state, &thread_id) else {
        return api_error(
            StatusCode::NOT_FOUND,
            "thread_not_found",
            format!("thread {thread_id} is not known"),
        );
    };
    let scope = loom_relay::Scope::Thread(thread_id.to_string());
    let frame = json!({
        "type": "thread_pane_action_requested",
        "threadId": thread.id.to_string(),
        "projectId": thread.project_id.to_string(),
        "action": request.action,
        "atMs": loom_relay::now_ms(),
    });
    let payload = serde_json::to_vec(&frame).expect("a pane action always serializes");
    if let Err(error) = state.publish(scope.clone(), payload) {
        return api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            error.to_string(),
        );
    }
    match state.hub.subscriber_count(scope).await {
        Ok(delivered) => Json(json!({ "delivered": delivered })).into_response(),
        Err(error) => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            error.to_string(),
        ),
    }
}

/// `threads.rawFile`: an HTML preview read from an absolute host path.
pub async fn thread_raw_file(
    State(state): State<AppState>,
    AxumPath(raw_thread_id): AxumPath<String>,
    Query(query): Query<ContentPathQuery>,
) -> Response {
    let (thread, environment) = match thread_and_environment(&state, &raw_thread_id) {
        Ok(pair) => pair,
        Err(response) => return response,
    };
    let _ = thread;
    if let Err(PathError::Invalid) = validate_absolute_path(&query.path) {
        return invalid_path_response();
    }
    // A raw filesystem route renders HTML, so refuse a non-HTML path before
    // asking the host for bytes that would be rejected anyway.
    if !query.path.to_ascii_lowercase().ends_with(".html") {
        return api_error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsupported_media_type",
            "HTML preview only supports text/html files",
        );
    }
    let outcome = match state
        .request_host_file(
            &environment.host_id,
            HostFileOperation::Read {
                path: query.path.clone(),
                root_path: None,
                max_bytes: MAX_HTML_PREVIEW_BYTES,
            },
        )
        .await
    {
        Ok(outcome) => outcome,
        Err(error) => return transport_error_response(error),
    };
    let relative = Path::new(&query.path)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned());
    content_from_outcome(outcome, relative.as_deref())
}

/// `threads.hostFileContent`: an absolute host path's bytes.
pub async fn thread_host_file_content(
    State(state): State<AppState>,
    AxumPath(raw_thread_id): AxumPath<String>,
    Query(query): Query<ContentPathQuery>,
) -> Response {
    let (_, environment) = match thread_and_environment(&state, &raw_thread_id) {
        Ok(pair) => pair,
        Err(response) => return response,
    };
    if let Err(PathError::Invalid) = validate_absolute_path(&query.path) {
        return invalid_path_response();
    }
    let outcome = match state
        .request_host_file(
            &environment.host_id,
            HostFileOperation::Read {
                path: query.path.clone(),
                root_path: None,
                max_bytes: MAX_FILE_CONTENT_BYTES,
            },
        )
        .await
    {
        Ok(outcome) => outcome,
        Err(error) => return transport_error_response(error),
    };
    content_from_outcome(outcome, None)
}

/// `threads.worktreeFile`: a root-relative path inside the workspace.
pub async fn thread_worktree_file(
    State(state): State<AppState>,
    AxumPath((raw_thread_id, raw_file_path)): AxumPath<(String, String)>,
) -> Response {
    let (thread, _) = match thread_and_environment(&state, &raw_thread_id) {
        Ok(pair) => pair,
        Err(response) => return response,
    };
    let Ok(relative) = validate_relative_path(&raw_file_path) else {
        return invalid_path_response();
    };
    let (environment, workspace) = match workspace_root(&state, &thread) {
        Ok(pair) => pair,
        Err(response) => return response,
    };
    let path = join_root(&workspace, relative);
    let outcome = match state
        .request_host_file(
            &environment.host_id,
            HostFileOperation::Read {
                path,
                root_path: Some(workspace),
                max_bytes: MAX_FILE_CONTENT_BYTES,
            },
        )
        .await
    {
        Ok(outcome) => outcome,
        Err(error) => return transport_error_response(error),
    };
    content_from_outcome(outcome, Some(relative))
}

/// `threads.storageContent`: a root-relative path inside thread storage.
pub async fn thread_storage_content(
    State(state): State<AppState>,
    AxumPath(raw_thread_id): AxumPath<String>,
    Query(query): Query<ContentPathQuery>,
) -> Response {
    let (thread, _) = match thread_and_environment(&state, &raw_thread_id) {
        Ok(pair) => pair,
        Err(response) => return response,
    };
    let Ok(relative) = validate_relative_path(&query.path) else {
        return invalid_path_response();
    };
    let (host_id, root) = match storage_target(&state, &thread) {
        Ok(target) => target,
        Err(response) => return response,
    };
    let path = join_root(&root, relative);
    let outcome = match state
        .request_host_file(
            &host_id,
            HostFileOperation::Read {
                path,
                root_path: Some(root),
                max_bytes: MAX_FILE_CONTENT_BYTES,
            },
        )
        .await
    {
        Ok(outcome) => outcome,
        Err(error) => return transport_error_response(error),
    };
    content_from_outcome(outcome, Some(relative))
}

/// `threads.storageFile`: the same read, with the path in the URL.
pub async fn thread_storage_file(
    State(state): State<AppState>,
    AxumPath((raw_thread_id, raw_file_path)): AxumPath<(String, String)>,
) -> Response {
    let (thread, _) = match thread_and_environment(&state, &raw_thread_id) {
        Ok(pair) => pair,
        Err(response) => return response,
    };
    let Ok(relative) = validate_relative_path(&raw_file_path) else {
        return invalid_path_response();
    };
    let (host_id, root) = match storage_target(&state, &thread) {
        Ok(target) => target,
        Err(response) => return response,
    };
    let path = join_root(&root, relative);
    let outcome = match state
        .request_host_file(
            &host_id,
            HostFileOperation::Read {
                path,
                root_path: Some(root),
                max_bytes: MAX_FILE_CONTENT_BYTES,
            },
        )
        .await
    {
        Ok(outcome) => outcome,
        Err(error) => return transport_error_response(error),
    };
    content_from_outcome(outcome, Some(relative))
}

/// `threads.storageFiles`: thread storage's files.
pub async fn thread_storage_files(
    State(state): State<AppState>,
    AxumPath(raw_thread_id): AxumPath<String>,
    Query(query): Query<FileListQuery>,
) -> Response {
    let (thread, _) = match thread_and_environment(&state, &raw_thread_id) {
        Ok(pair) => pair,
        Err(response) => return response,
    };
    let limit = match parse_limit(query.limit.as_ref()) {
        Ok(limit) => limit,
        Err(response) => return response,
    };
    let (host_id, root) = match storage_target(&state, &thread) {
        Ok(target) => target,
        Err(response) => return response,
    };
    let outcome = match state
        .request_host_file(
            &host_id,
            HostFileOperation::List {
                path: root.clone(),
                query: query.query,
                limit,
                include_files: true,
                include_directories: false,
                include_hidden: false,
            },
        )
        .await
    {
        Ok(outcome) => outcome,
        Err(error) => return transport_error_response(error),
    };
    match outcome {
        HostFileOutcome::Listing { entries, truncated } => Json(json!({
            // `fileSchema` carries only `path` and `name`; the scored path
            // entry shape belongs to `storagePaths`.
            "files": entries.iter().map(|entry| entry_value(entry, false)).collect::<Vec<_>>(),
            "truncated": truncated,
            "storageRootPath": root,
        }))
        .into_response(),
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
        HostFileOutcome::Failed { code, message } => host_failure_response(&code, &message),
    }
}

/// `threads.storagePaths`: thread storage's files and/or directories.
pub async fn thread_storage_paths(
    State(state): State<AppState>,
    AxumPath(raw_thread_id): AxumPath<String>,
    Query(query): Query<PathListQuery>,
) -> Response {
    let (thread, _) = match thread_and_environment(&state, &raw_thread_id) {
        Ok(pair) => pair,
        Err(response) => return response,
    };
    let include_files = query.include_files == "true";
    let include_directories = query.include_directories == "true";
    if !include_files && !include_directories {
        return api_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "At least one path kind must be included",
        );
    }
    let limit = match parse_limit(query.limit.as_ref()) {
        Ok(limit) => limit,
        Err(response) => return response,
    };
    let (host_id, root) = match storage_target(&state, &thread) {
        Ok(target) => target,
        Err(response) => return response,
    };
    let outcome = match state
        .request_host_file(
            &host_id,
            HostFileOperation::List {
                path: root.clone(),
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
        Err(error) => return transport_error_response(error),
    };
    match outcome {
        HostFileOutcome::Listing { entries, truncated } => Json(json!({
            "paths": entries.iter().map(|entry| entry_value(entry, true)).collect::<Vec<_>>(),
            "truncated": truncated,
            "storageRootPath": root,
        }))
        .into_response(),
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
        HostFileOutcome::Failed { code, message } => host_failure_response(&code, &message),
    }
}

/// `threads.storageLocation`: which host owns the storage, and where it is.
///
/// Answered from the entity view, not from the host: it is a question about the
/// layout, and a client opening a storage panel should not fail because the
/// worker is momentarily away. The directory itself need not exist yet.
pub async fn thread_storage_location(
    State(state): State<AppState>,
    AxumPath(raw_thread_id): AxumPath<String>,
) -> Response {
    let (thread, _) = match thread_and_environment(&state, &raw_thread_id) {
        Ok(pair) => pair,
        Err(response) => return response,
    };
    match storage_target(&state, &thread) {
        Ok((host_id, root)) => Json(json!({
            "hostId": host_id.to_string(),
            "storageRootPath": root,
        }))
        .into_response(),
        Err(response) => response,
    }
}

/* ------------------------------------------------------------------ */
/* Shared helpers                                                      */
/* ------------------------------------------------------------------ */

/// Loads a public thread and the environment its file routes act on.
fn thread_and_environment(
    state: &AppState,
    raw_thread_id: &str,
) -> Result<(Thread, Environment), Response> {
    let Ok(thread_id) = raw_thread_id.parse::<ThreadId>() else {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "invalid thread id",
        ));
    };
    let thread = public_thread(state, &thread_id)?;
    let environment = ready_environment(state, &thread)?;
    Ok((thread, environment))
}

fn public_thread(state: &AppState, thread_id: &ThreadId) -> Result<Thread, Response> {
    match state.registry.thread(thread_id) {
        Some(thread) if thread.deleted_at_ms.is_none() => Ok(thread),
        Some(_) => Err(api_error(
            StatusCode::NOT_FOUND,
            "thread_not_found",
            format!("thread {thread_id} has been deleted"),
        )),
        None => Err(api_error(
            StatusCode::NOT_FOUND,
            "thread_not_found",
            format!("thread {thread_id} is not known"),
        )),
    }
}

/// Standard base64 (RFC 4648) encoding, so an upload can carry binary bytes
/// across the JSON host protocol.
///
/// Hand-rolled for the same reason the decoder is: the control plane needs two
/// directions of one encoding, and a dependency for that is not worth the
/// supply-chain surface. The worker has its own copy on the other side of the
/// wire.
pub(crate) fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        encoded.push(ALPHABET[(first >> 2) as usize] as char);
        encoded.push(ALPHABET[(((first & 0b11) << 4) | (second >> 4)) as usize] as char);
        encoded.push(if chunk.len() > 1 {
            ALPHABET[(((second & 0b1111) << 2) | (third >> 6)) as usize] as char
        } else {
            '='
        });
        encoded.push(if chunk.len() > 2 {
            ALPHABET[(third & 0b11_1111) as usize] as char
        } else {
            '='
        });
    }
    encoded
}

/// Standard base64 (RFC 4648) decoding, used to turn a worker's binary payload
/// back into bytes.
pub(crate) fn base64_decode(raw: &str) -> Option<Vec<u8>> {
    let mut decoded = Vec::with_capacity(raw.len() / 4 * 3);
    let mut buffer = 0u32;
    let mut bits = 0u32;
    for byte in raw.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' | b'\n' | b'\r' => continue,
            _ => return None,
        };
        buffer = (buffer << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            decoded.push((buffer >> bits) as u8);
        }
    }
    Some(decoded)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_relative_path_may_not_climb_out_of_its_root() {
        for bad in [
            "",
            "..",
            "../etc/passwd",
            "a/../../b",
            ".",
            "a/./b",
            "a//b",
            "/etc/passwd",
            "a\\..\\b",
            "a\0b",
        ] {
            assert!(
                validate_relative_path(bad).is_err(),
                "{bad:?} should be refused"
            );
        }
        assert_eq!(
            validate_relative_path("src/main.rs").unwrap(),
            "src/main.rs"
        );
        assert_eq!(validate_relative_path(".env").unwrap(), ".env");
    }

    #[test]
    fn an_absolute_path_only_refuses_what_cannot_be_one() {
        assert!(validate_absolute_path("relative/x").is_err());
        assert!(validate_absolute_path("").is_err());
        assert!(validate_absolute_path("/tmp/a\0b").is_err());
        assert_eq!(
            validate_absolute_path("/tmp/log.txt").unwrap(),
            "/tmp/log.txt"
        );
        // A Windows host is a supported host, so its spelling is accepted.
        assert_eq!(
            validate_absolute_path("C:\\Temp\\log.txt").unwrap(),
            "C:\\Temp\\log.txt"
        );
    }

    #[test]
    fn joining_a_root_and_a_relative_path_always_uses_slashes() {
        assert_eq!(
            join_root("/var/lib/loom", "a/b.txt"),
            "/var/lib/loom/a/b.txt"
        );
        // A root reported with a trailing separator must not produce a double.
        assert_eq!(join_root("/var/lib/loom/", "a"), "/var/lib/loom/a");
        assert_eq!(
            thread_storage_root("/var/lib/loom", "thr_1"),
            "/var/lib/loom/thread-storage/thr_1"
        );
    }

    #[test]
    fn base64_round_trips_binary_content() {
        let bytes: Vec<u8> = (0u8..=255).collect();
        let encoded = base64_encode(&bytes);
        assert_eq!(base64_decode(&encoded).unwrap(), bytes);
        // Padding: a two-byte tail decodes without trailing garbage.
        assert_eq!(base64_decode("YQ==").unwrap(), b"a");
        assert_eq!(base64_decode("YWI=").unwrap(), b"ab");
        assert!(base64_decode("not base64!").is_none());
    }
}
