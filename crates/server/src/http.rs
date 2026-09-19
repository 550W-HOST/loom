//! HTTP surface.
//!
//! Small on purpose. The interesting surface is the WebSocket; these routes
//! exist so a server can be probed, identified and fed events.

use std::collections::HashSet;

use axum::body::Body;
use axum::extract::{Path, Query, Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use http_body_util::BodyExt;
use loom_domain::{
    catalog::{CatalogModel, ProviderCatalog},
    DomainError, DomainEvent, DomainScope, Environment, EnvironmentId, EnvironmentKind,
    EnvironmentStatus, Host, HostId, HostStatus, Interaction, InteractionId, InteractionOrigin,
    MessageRole, NewQueuedMessage, Project, ProjectId, ProjectKind, ProjectSourceId, QueuedMessage,
    QueuedMessageId, QueuedMessageInitiator, QueuedMessagePayload, QueuedMessageStatus,
    ReasoningLevel, Resolution, ServiceTier, Thread, ThreadId, ThreadStatus, ThreadTrigger,
    ThreadUpdate,
};
use loom_provider_protocol::ProviderSpec;
use loom_relay::scope::Scope;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::domain_state::CommandError;
use crate::interactions::DeliverOutcome;
use crate::queue::DeliveryOutcome;
use crate::runs::DispatchOutcome;
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
            "/api/v1/settings/appearance",
            put(crate::b10::update_appearance),
        )
        .route(
            "/api/v1/settings/experiments",
            put(crate::b10::update_experiments),
        )
        .route("/api/v1/settings/general", put(crate::b10::update_general))
        .route(
            "/api/v1/settings/keyboard",
            put(crate::b10::update_keyboard),
        )
        .route(
            "/api/v1/settings/themes/{id}",
            get(crate::b10::resolve_theme),
        )
        .route("/api/v1/settings/themes", get(crate::b10::themes))
        .route("/api/v1/preferences/ui", get(crate::b10::ui_preferences))
        .route(
            "/api/v1/preferences/ui/{key}",
            put(crate::b10::update_ui_preference).delete(crate::b10::reset_ui_preference),
        )
        .route(
            "/api/v1/system/providers/{id}/logo",
            get(crate::b10::provider_logo),
        )
        .route(
            "/api/v1/system/config/reload",
            post(crate::b10::reload_config),
        )
        .route("/api/v1/system/usage-limits", get(crate::b10::usage_limits))
        .route(
            "/api/v1/system/voice-transcription",
            post(crate::b10::voice_transcription),
        )
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
        .route("/api/v1/threads/fork", post(fork_thread))
        // B5: thread files, pane actions and thread storage. Declared with
        // literal paths, in contract order, so `check-api-coverage.mjs` can
        // parse them and the routes stay next to their siblings.
        .route("/api/v1/threads/count", get(crate::b5::thread_count))
        .route(
            "/api/v1/threads/resolve-mentions",
            post(resolve_thread_mentions),
        )
        .route("/api/v1/threads/running", get(running_threads))
        .route("/api/v1/threads/search", get(search_threads))
        .route("/api/v1/threads/{id}/events", get(thread_events))
        .route(
            "/api/v1/threads/{id}",
            get(get_thread).patch(update_thread).delete(delete_thread),
        )
        .route("/api/v1/threads/{id}/output", get(thread_output))
        .route("/api/v1/threads/{id}/read", post(read_thread))
        .route("/api/v1/threads/{id}/unread", post(mark_thread_unread))
        .route("/api/v1/threads/{id}/archive", post(archive_thread))
        .route(
            "/api/v1/threads/{id}/archive-all",
            post(archive_all_threads),
        )
        .route("/api/v1/threads/{id}/unarchive", post(unarchive_thread))
        .route("/api/v1/threads/{id}/pin", post(pin_thread))
        .route(
            "/api/v1/threads/{id}/pane-action",
            post(crate::b5::thread_pane_action),
        )
        .route(
            "/api/v1/threads/{id}/files/raw",
            get(crate::b5::thread_raw_file),
        )
        .route(
            "/api/v1/threads/{id}/host-files/content",
            get(crate::b5::thread_host_file_content),
        )
        .route(
            "/api/v1/threads/{id}/thread-storage/location",
            get(crate::b5::thread_storage_location),
        )
        .route(
            "/api/v1/threads/{id}/thread-storage/files",
            get(crate::b5::thread_storage_files),
        )
        .route(
            "/api/v1/threads/{id}/thread-storage/paths",
            get(crate::b5::thread_storage_paths),
        )
        .route(
            "/api/v1/threads/{id}/thread-storage/content",
            get(crate::b5::thread_storage_content),
        )
        // The two path-in-URL reads are declared with axum's `{*path}`
        // wildcard, which consumes every remaining segment including
        // separators — the same capture the contract writes as
        // `:filePath{.+}`. `check-api-coverage.mjs` normalises any braced
        // segment to a parameter, so the two spellings match.
        .route(
            "/api/v1/threads/{id}/thread-storage/files/{*file_path}",
            get(crate::b5::thread_storage_file),
        )
        .route(
            "/api/v1/threads/{id}/worktree/files/{*file_path}",
            get(crate::b5::thread_worktree_file),
        )
        .route("/api/v1/threads/{id}/unpin", post(unpin_thread))
        .route(
            "/api/v1/threads/{id}/pin-order",
            axum::routing::patch(reorder_pinned_thread),
        )
        .route("/api/v1/threads/{id}/send", post(send_thread))
        .route(
            "/api/v1/threads/{id}/child-summary",
            get(thread_child_summary),
        )
        .route("/api/v1/threads/{id}/compact", post(compact_thread))
        .route(
            "/api/v1/threads/{id}/conversation-outline",
            get(thread_conversation_outline),
        )
        .route(
            "/api/v1/threads/{id}/default-execution-options",
            get(thread_default_execution_options),
        )
        .route(
            "/api/v1/threads/{id}/edit-message",
            post(edit_thread_message),
        )
        .route("/api/v1/threads/{id}/open", post(open_thread))
        .route(
            "/api/v1/threads/{id}/prompt-history",
            get(thread_prompt_history),
        )
        .route("/api/v1/threads/{id}/retry", post(retry_thread))
        .route("/api/v1/threads/{id}/stop", post(stop_thread_route))
        .route(
            "/api/v1/threads/{id}/tabs",
            get(thread_tabs).put(update_thread_tabs),
        )
        .route("/api/v1/threads/{id}/timeline", get(thread_timeline))
        .route(
            "/api/v1/threads/{id}/timeline/turn-summary-details",
            get(thread_turn_summary_details),
        )
        .route("/api/v1/threads/{id}/events/wait", get(thread_event_wait))
        .route("/api/v1/threads/{id}/goal/clear", post(clear_thread_goal))
        .route(
            "/api/v1/threads/{id}/context/clear",
            post(clear_thread_context),
        )
        .route("/api/v1/threads/{id}/plan/cancel", post(cancel_thread_plan))
        .route(
            "/api/v1/threads/{id}/interactions",
            get(thread_interactions),
        )
        .route(
            "/api/v1/threads/{id}/interactions/{interaction_id}",
            get(thread_interaction),
        )
        .route(
            "/api/v1/threads/{id}/interactions/{interaction_id}/resolve",
            post(resolve_thread_interaction),
        )
        .route(
            "/api/v1/threads/{id}/interactions/{interaction_id}/respond",
            post(respond_to_thread_interaction),
        )
        .route(
            "/api/v1/threads/{id}/interactions/{interaction_id}/cancel",
            post(cancel_thread_interaction),
        )
        .route(
            "/api/v1/threads/{id}/queued-messages",
            get(thread_queued_messages).post(create_queued_message),
        )
        .route(
            "/api/v1/threads/{id}/queued-messages/{queued_message_id}/send",
            post(send_queued_message),
        )
        .route(
            "/api/v1/threads/{id}/queued-messages/{queued_message_id}",
            axum::routing::delete(delete_queued_message).patch(update_queued_message),
        )
        .route(
            "/api/v1/threads/{id}/queued-messages/{queued_message_id}/order",
            axum::routing::patch(reorder_queued_message),
        )
        .route(
            "/api/v1/threads/{id}/queued-messages/group-boundary",
            axum::routing::patch(set_queued_message_group_boundary),
        )
        .route("/api/v1/queued-messages", get(list_queued_messages))
        .route("/api/v1/threads/{id}/messages", post(post_thread_message))
        .route("/api/v1/projects", get(list_projects).post(create_project))
        .route(
            "/api/v1/projects/{id}",
            get(get_project)
                .patch(update_project)
                .delete(crate::b7::project_delete),
        )
        .route(
            "/api/v1/projects/{id}/default-execution-options",
            get(project_default_execution_options),
        )
        .route("/api/v1/projects/{id}/archive", post(archive_project))
        .route("/api/v1/projects/{id}/sources", post(add_project_source))
        .route(
            "/api/v1/projects/{id}/sources/{source_id}",
            axum::routing::delete(remove_project_source).patch(crate::b7::project_update_source),
        )
        .route(
            "/api/v1/projects/{id}/branches",
            get(crate::b6::project_branches),
        )
        .route(
            "/api/v1/projects/{id}/branch-options",
            get(crate::b6::project_branch_options),
        )
        .route("/api/v1/projects/{id}/files", get(crate::b7::project_files))
        .route(
            "/api/v1/projects/{id}/files/content",
            get(crate::b7::project_file_content),
        )
        .route("/api/v1/projects/{id}/paths", get(crate::b7::project_paths))
        .route(
            "/api/v1/projects/{id}/commands",
            get(crate::b7::project_commands),
        )
        .route(
            "/api/v1/projects/{id}/attachments",
            post(crate::b7::project_upload_attachment),
        )
        .route(
            "/api/v1/projects/{id}/attachments/content",
            get(crate::b7::project_attachment_content),
        )
        .route(
            "/api/v1/projects/{id}/attachments/copy",
            post(crate::b7::project_copy_attachments),
        )
        .route(
            "/api/v1/projects/{id}/prompt-history",
            get(crate::b7::project_prompt_history),
        )
        .route(
            "/api/v1/projects/{id}/order",
            axum::routing::patch(crate::b7::project_reorder),
        )
        .route(
            "/api/v1/thread-sections",
            post(crate::b7::create_thread_section)
                .patch(crate::b7::update_thread_section)
                .delete(crate::b7::delete_thread_section),
        )
        .route(
            "/api/v1/environments",
            get(list_environments).post(create_environment),
        )
        .route(
            "/api/v1/environments/{id}",
            get(get_environment)
                .patch(crate::b6::update_environment)
                .delete(crate::b6::delete_environment),
        )
        .route(
            "/api/v1/environments/{id}/actions",
            post(crate::b6::environment_actions),
        )
        .route(
            "/api/v1/environments/{id}/archive-threads",
            post(crate::b6::archive_environment_threads),
        )
        .route(
            "/api/v1/environments/{id}/paths",
            get(crate::b6::environment_paths),
        )
        .route(
            "/api/v1/environments/{id}/status",
            get(crate::b6::environment_status),
        )
        .route(
            "/api/v1/environments/{id}/diff",
            get(crate::b6::environment_diff),
        )
        .route(
            "/api/v1/environments/{id}/diff/branches",
            get(crate::b6::environment_diff_branches),
        )
        .route(
            "/api/v1/environments/{id}/diff/file",
            get(crate::b6::environment_diff_file),
        )
        .route(
            "/api/v1/environments/{id}/diff/files",
            get(crate::b6::environment_diff_files),
        )
        .route(
            "/api/v1/environments/{id}/diff/patch",
            post(crate::b6::environment_diff_patch),
        )
        .route(
            "/api/v1/environments/{id}/pull-request",
            get(crate::b6::environment_pull_request),
        )
        .route(
            "/api/v1/environments/{id}/provision",
            post(provision_environment),
        )
        .route(
            "/api/v1/environments/{id}/destroy",
            post(destroy_environment),
        )
        .route("/api/v1/runs", get(list_runs))
        // Automations are loom-native: bb's contract has no route for them, so
        // these are declared, and documented, as contract-external. The scope
        // is project + automation id, which is what the contract's inputs name.
        .route("/api/v1/automations", get(crate::automations::overview))
        .route(
            "/api/v1/projects/{id}/automations",
            get(crate::automations::list).post(crate::automations::create),
        )
        .route(
            "/api/v1/projects/{id}/automations/{automationId}",
            get(crate::automations::get)
                .patch(crate::automations::update)
                .delete(crate::automations::delete),
        )
        .route(
            "/api/v1/projects/{id}/automations/{automationId}/pause",
            post(crate::automations::pause),
        )
        .route(
            "/api/v1/projects/{id}/automations/{automationId}/resume",
            post(crate::automations::resume),
        )
        .route(
            "/api/v1/projects/{id}/automations/{automationId}/run",
            post(crate::automations::run),
        )
        .route(
            "/api/v1/projects/{id}/automations/{automationId}/runs",
            get(crate::automations::runs),
        )
        .route("/api/v1/hosts", get(list_hosts).post(register_host))
        .route(
            "/api/v1/hosts/join-codes",
            post(crate::b8::create_join_code),
        )
        .route("/api/v1/hosts/primary", get(primary_host))
        .route("/api/v1/hosts/{id}/heartbeat", post(host_heartbeat))
        .route("/api/v1/hosts/{id}/disconnect", post(disconnect_host))
        .route(
            "/api/v1/hosts/{id}",
            get(crate::b8::host_get)
                .patch(crate::b8::host_update)
                .delete(crate::b8::host_delete),
        )
        .route(
            "/api/v1/hosts/{id}/permission-ceiling",
            axum::routing::patch(crate::b8::host_permission_ceiling),
        )
        .route(
            "/api/v1/hosts/{id}/directory",
            get(crate::b8::host_directory),
        )
        .route(
            "/api/v1/hosts/{id}/paths/exist",
            post(crate::b8::host_paths_exist),
        )
        .route(
            "/api/v1/hosts/{id}/pick-folder",
            post(crate::b8::host_pick_folder),
        )
        .route(
            "/api/v1/hosts/{id}/clone-default-path",
            get(crate::b8::host_clone_default_path),
        )
        .route(
            "/api/v1/hosts/{id}/provider-clis/status",
            get(crate::b8::provider_cli_status),
        )
        .route(
            "/api/v1/hosts/{id}/provider-clis/install",
            post(crate::b8::provider_cli_install),
        )
        .route(
            "/api/v1/hosts/{id}/retry-update",
            post(crate::b8::host_retry_update),
        )
        .route("/api/v1/system/attention", get(crate::b8::system_attention))
        .route(
            "/api/v1/files/previews",
            post(crate::b8::file_preview_create),
        )
        .route(
            "/api/v1/file-previews/{id}/{*file_path}",
            get(crate::b8::file_preview_content),
        )
        // B9: workspace file operations and terminal sessions. The file routes
        // are literal and the terminal routes share one path prefix with a
        // parameter, so `check-api-coverage.mjs` can parse them.
        .route("/api/v1/files/list", post(crate::b9::files_list))
        .route("/api/v1/files/paths", post(crate::b9::files_list_paths))
        .route("/api/v1/files/mkdir", post(crate::b9::files_mkdir))
        .route("/api/v1/files/move", post(crate::b9::files_move))
        .route("/api/v1/files/read", post(crate::b9::files_read))
        .route("/api/v1/files/remove", post(crate::b9::files_remove))
        .route("/api/v1/files/write", post(crate::b9::files_write))
        .route(
            "/api/v1/terminals",
            get(crate::b9::terminals_list).post(crate::b9::terminals_create),
        )
        .route(
            "/api/v1/terminals/{terminal_id}",
            get(crate::b9::terminals_get).patch(crate::b9::terminals_update),
        )
        .route(
            "/api/v1/terminals/{terminal_id}/input",
            post(crate::b9::terminals_input),
        )
        .route(
            "/api/v1/terminals/{terminal_id}/output",
            get(crate::b9::terminals_output),
        )
        .route(
            "/api/v1/terminals/{terminal_id}/resize",
            post(crate::b9::terminals_resize),
        )
        .route(
            "/api/v1/terminals/{terminal_id}/close",
            post(crate::b9::terminals_close),
        )
        .route(
            "/api/v1/terminals/{terminal_id}/restart",
            post(crate::b9::terminals_restart),
        )
        .route("/ws", get(ws::client_socket))
        .route("/internal/ws", get(ws::worker_socket))
        // Worker self-update: the version to compare against, and the binary
        // that matches it. Deliberately not behind the `/api` namespace — a
        // worker fetching its own replacement is not a domain operation — and
        // declared here as literals so `scripts/check-api-coverage.mjs` can
        // parse them and record them as the contract-external entries they are
        // (`artifacts::INSTALL_VERSION_PATH` and its sibling are what the
        // handlers and the worker client use).
        .route("/install/version", get(artifacts::install_version))
        .route("/install/loom-worker", get(artifacts::install_worker))
        // Everything else is a client route: the UI shell (or a dev-server
        // proxy). API and socket paths are excluded inside the handler.
        .fallback(ui::serve)
        .layer(middleware::from_fn(validate_contract_request))
        .layer(middleware::from_fn(validate_automation_request))
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
    let mut instance: Value = match serde_json::from_slice(&bytes) {
        Ok(instance) => instance,
        Err(_) => {
            return next
                .run(Request::from_parts(parts, Body::from(bytes)))
                .await;
        }
    };
    // loom serves the agent's own reasoning-level ids — `off`, `minimal` — while
    // bb's request schemas still name only its own eight, so the level's *value*
    // is not something loom can validate against that vocabulary. A body that
    // carries one is validated with a legal placeholder in its place: the bytes
    // handed to the handler are untouched, an omitted level is still missing,
    // and the type is still enforced by serde.
    if let Some(object) = instance.as_object_mut() {
        if object.contains_key("reasoningLevel") {
            object.insert("reasoningLevel".into(), json!("medium"));
        }
    }
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

/// Rejects an automations request body the contract does not describe.
///
/// bb's contract has no automations entries, so `validate_contract_request`
/// above cannot see these routes; the schemas in
/// [`crate::automations_contract`] are loom's own and this is where they run.
/// It exists for the same reason the bb middleware does: a handler that
/// silently accepts a key the contract does not name is a dialect no client can
/// discover, and the contract spells every one of these objects `.strict()` —
/// including the unions nested inside them, which serde cannot express.
///
/// It is a no-op for everything else: the read routes carry no body, and a
/// malformed body is the JSON extractor's to report.
async fn validate_automation_request(request: Request, next: Next) -> Response {
    let Some(operation) =
        crate::automations_contract::write_operation(request.method(), request.uri().path())
    else {
        return next.run(request).await;
    };
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
    let instance: Value = match serde_json::from_slice(&bytes) {
        Ok(instance) => instance,
        Err(_) => {
            return next
                .run(Request::from_parts(parts, Body::from(bytes)))
                .await;
        }
    };
    let violations = crate::automations_contract::validate_request(operation, &instance);
    if !violations.is_empty() {
        return error_response_with_code(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_request",
            format!(
                "request body does not match the automations contract: {}",
                crate::automations_contract::describe(&violations)
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

const PERSONAL_WORKSPACE_PROVIDER_ID: &str = "personal-workspace";
const PROJECT_CHECKOUT_PROVIDER_ID: &str = "project-checkout";

fn environment_provider_machine_availability(host: &Host, source_available: bool) -> Value {
    if host.status != HostStatus::Connected {
        return json!({
            "status": "unavailable",
            "message": "Machine is disconnected"
        });
    }
    if !source_available {
        return json!({
            "status": "unavailable",
            "message": "Project has no workspace source on this machine"
        });
    }
    json!({ "status": "available" })
}

fn aggregate_provider_availability(machine_availability: &serde_json::Map<String, Value>) -> Value {
    if machine_availability
        .values()
        .any(|value| value.get("status").and_then(Value::as_str) == Some("available"))
    {
        json!({ "status": "available" })
    } else {
        json!({
            "status": "setup-required",
            "message": "Connect an eligible machine first"
        })
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[allow(dead_code)]
struct SystemVersionQuery {
    force: Option<String>,
}

/// The default agent's id.
///
/// Used by routes whose subject does not record a provider yet — a thread, a
/// queued message, an interaction. Those were dispatched by the default
/// provider, so naming it is the honest answer until the record carries one of
/// its own.
fn configured_provider_id(state: &AppState) -> String {
    state.provider_spec().name.clone()
}

/// The agent a system query is about.
///
/// A request that names an offered provider gets it; one that names an unknown
/// id, or none at all, gets the default. The client only ever names a provider
/// this server advertised, so the fallback covers a stale tab rather than a
/// normal path.
fn query_provider(state: &AppState, query: &ProviderQuery) -> ProviderSpec {
    query
        .provider_id
        .as_deref()
        .and_then(|provider_id| state.provider_spec_by_id(provider_id))
        .unwrap_or_else(|| state.provider_spec())
}

/// The name an agent is shown under.
///
/// Cosmetic, and deliberately a table rather than a field on [`ProviderSpec`]:
/// that is the dispatch contract and carries launch metadata, not presentation.
/// The keys are the ids discovery uses, so a tab reads "OpenCode" rather than
/// "opencode" without the worker having to describe how its own name is
/// capitalised.
///
/// `pub(crate)` because the logo route draws a monogram from the same table: two
/// tables would eventually disagree about what a provider is called.
pub(crate) fn provider_display_name(provider_id: &str) -> String {
    match provider_id {
        "pi" => "Pi".to_owned(),
        "omp" => "OMP".to_owned(),
        "hermes" => "Hermes".to_owned(),
        "opencode" => "OpenCode".to_owned(),
        "gemini" => "Gemini".to_owned(),
        "cursor" => "Cursor".to_owned(),
        "codex" => "Codex".to_owned(),
        "claude-code" => "Claude Code".to_owned(),
        other => other.to_owned(),
    }
}

fn provider_info(spec: &ProviderSpec) -> Value {
    let provider_id = spec.name.clone();
    // bb addresses a mark by its content: an icon cannot change without its URL
    // changing, so the logo route can answer a matching request as immutable.
    let logo_url = format!(
        "/api/v1/system/providers/{provider_id}/logo?h={}",
        crate::b10::provider_mark_hash(&spec.name)
    );
    let mut info = json!({
        "id": provider_id,
        // `pluginId` remains a required bb field even though loom providers
        // are first-class and do not use a plugin lifecycle.
        "pluginId": "loom",
        "displayName": provider_display_name(&spec.name),
        // The client draws the provider mark from `logoUrl`, and keeps
        // `family` as the fallback it uses when no logo can be loaded.
        "logoUrl": logo_url,
        "family": "acp",
        "maintenance": {
            "health": true,
            "usage": true,
            "installation": false
        },
        "capabilities": {
            "supportsThreadArchive": true,
            // `threads.update` now applies a title change, so the client may
            // offer the affordance. The other flags stay honest about what
            // loom's provider protocol can do: no service tier, no rewind.
            "supportsThreadRename": true,
            "supportsServiceTier": false,
            "supportsNativeUserQuestion": false,
            "supportsFork": false,
            "supportsSessionRewind": false,
            "permissionModes": ["accept-edits", "auto", "full"],
            "modelCatalogScope": "workspace"
        },
        "composerActions": [],
        "available": true
    });
    // Omitted rather than null when an agent has no branding: the contract
    // allows either, but the client's schema is `optional()` and rejects an
    // explicit null, and a rejected object costs the whole provider list.
    if let Some(strings) = provider_strings(&spec.name) {
        info["strings"] = strings;
    }
    info
}

/// The presentation strings a provider carries, worded the way bb words them.
///
/// `strings` is where a provider says how to sign in, where to install it, and
/// what colour its mark is. Loom does not own an agent's credentials — the agent
/// signs itself in on its own machine — so a hint names that agent's own
/// command, which is also what bb shows for the same agents.
fn provider_strings(provider_id: &str) -> Option<Value> {
    let branding = crate::b10::provider_branding(provider_id)?;
    let name = provider_display_name(provider_id);
    let mut strings = json!({
        "signInHint": format!(
            "Run `{}` on the machine to sign in.",
            branding.sign_in_command
        ),
        "expiredHint": format!(
            "Your {name} session expired. Run `{}`, then reload.",
            branding.sign_in_command
        ),
        "installUrl": branding.install_url,
    });
    // Most agents carry one brand colour for both themes and a few carry none;
    // only Cursor publishes a real pair, and `light-dark()` is what lets the
    // client resolve it with the rest of the app's theming.
    if let (Some(light), Some(dark)) = (branding.light, branding.dark) {
        strings["iconTint"] = json!({ "light": light, "dark": dark });
    }
    Some(strings)
}

fn configured_model(provider_id: &str) -> Value {
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

/// One model as the client's catalogue sees it (`d71`).
///
/// Every id here is the agent's own: the client sends `model` back verbatim as
/// the model choice, so a translated value would select something the agent
/// never named. `id` is only the catalogue entry's identity, namespaced by
/// provider so two agents cannot collide.
fn catalog_model(model: &CatalogModel, provider_id: &str, is_default: bool) -> Value {
    let supported: Vec<Value> = model
        .thinking_levels
        .iter()
        .map(|level| {
            json!({
                "reasoningEffort": level.id,
                // The client picks this up by id; the description is what it
                // shows when the level is not in its own label table.
                "description": level
                    .description
                    .clone()
                    .unwrap_or_else(|| level.name.clone()),
            })
        })
        .collect();
    // The agent's stated default is authoritative. When it named none, the
    // first level of its own ladder is the honest answer, and `medium` is the
    // last resort for a model that described no ladder at all.
    let default_effort = model
        .default_thinking_level
        .clone()
        .or_else(|| model.thinking_levels.first().map(|level| level.id.clone()))
        .unwrap_or_else(|| "medium".to_owned());
    json!({
        "id": format!("{provider_id}/{}", model.id),
        "model": model.id,
        "displayName": model.name,
        "description": model.name,
        "supportedReasoningEfforts": supported,
        "defaultReasoningEffort": default_effort,
        "isDefault": is_default,
    })
}

/// The models an agent advertised, in the shape `d71` asks for.
///
/// `current_model` names which entry is the session's own; when the agent named
/// one that is not in its list, nothing is marked and the client falls back to
/// the first entry rather than a lie.
fn catalog_models(catalog: &ProviderCatalog, provider_id: &str) -> Vec<Value> {
    catalog
        .models
        .iter()
        .map(|model| {
            catalog_model(
                model,
                provider_id,
                catalog.current_model.as_deref() == Some(model.id.as_str()),
            )
        })
        .collect()
}

/// The catalogue a query's host reported for one provider, when there is one.
///
/// A named host's own answer is the only acceptable one: answering with another
/// machine's models would offer choices the agent that will run the thread does
/// not have. The provider is part of the key for the same reason — two agents
/// on one machine advertise different models. A query that named no host — a
/// fresh install's first composer call, before an environment is chosen —
/// takes the newest report for that provider, which is what lets the picker
/// show real models before anything has run.
fn catalog_for_query(
    state: &AppState,
    provider_id: &str,
    query: &ProviderQuery,
) -> Option<ProviderCatalog> {
    if let Some(host_id) = query
        .host_id
        .as_deref()
        .and_then(|raw| raw.parse::<HostId>().ok())
    {
        return state
            .catalogs
            .get(&host_id, provider_id)
            .filter(|it| !it.is_empty());
    }
    if let Some(environment_id) = query
        .environment_id
        .as_deref()
        .and_then(|raw| raw.parse::<EnvironmentId>().ok())
    {
        let host_id = state.registry.environment(&environment_id)?.host_id;
        return state
            .catalogs
            .get(&host_id, provider_id)
            .filter(|it| !it.is_empty());
    }
    state.catalogs.most_recent(provider_id)
}

/// The execution options one provider resolves to.
///
/// `providers` lists every configured agent, because that is the list the
/// composer's provider picker draws its tabs from; `models` are the ones the
/// request's provider advertised, so a tab that is not the selected one never
/// leaks its models into another's catalogue.
fn execution_options_for(
    state: &AppState,
    provider: &ProviderSpec,
    catalog: Option<&ProviderCatalog>,
) -> Value {
    let providers: Vec<Value> = state.providers().iter().map(provider_info).collect();
    if let Some(catalog) = catalog.filter(|catalog| !catalog.is_empty()) {
        let models = catalog_models(catalog, &provider.name);
        return json!({
            "providers": providers,
            "permissionCeiling": "full",
            "models": models,
            "selectedOnlyModels": models,
            "modelLoadError": null,
        });
    }
    // No host has described this agent yet. The hardcoded entry keeps the
    // picker non-empty, and is replaced the moment a worker reports its
    // catalogue.
    let model = configured_model(&provider.name);
    json!({
        "providers": providers,
        "permissionCeiling": "full",
        "models": [model.clone()],
        "selectedOnlyModels": [model],
        "modelLoadError": null
    })
}

/// Returns the project/thread data needed to hydrate the sidebar in one call.
#[allow(clippy::result_large_err)]
async fn sidebar_bootstrap(State(state): State<AppState>) -> Json<Value> {
    // A deleted project is a tombstone: it still resolves by id for threads
    // and events, but it is not part of the list a client renders.
    let all = state
        .registry
        .projects()
        .into_iter()
        .filter(|project| !project.is_deleted())
        .collect::<Vec<_>>();
    // The personal project is a scope, not a list entry: the client renders it
    // from `personalProject` and addresses it by the reserved `proj_personal`.
    // Listing it under `projects` as well showed the same project twice, and
    // the row under `projects` was one the client did not recognise as
    // personal — a minted id it had never seen before.
    let personal_id = state.registry.personal_project_id();
    let personal_project = all
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
    let projects = all
        .iter()
        .filter(|project| project.id != personal_id)
        .map(|project| project_detail_value(&state, project))
        .collect::<Vec<_>>();
    // Sections are durable entities now (`threadSections.*`), so the sidebar's
    // own list is read from the registry rather than hardcoded empty.
    let sections = state
        .registry
        .thread_sections()
        .iter()
        .map(crate::b7::section_value)
        .collect::<Vec<_>>();
    Json(json!({
        "sections": sections,
        "projects": projects,
        "personalProject": personal_project
    }))
}

/// Returns the static configuration surface required by the bb client.
async fn system_config(State(state): State<AppState>) -> Json<Value> {
    let settings = state.settings.export();
    Json(json!({
        "generalSettings": crate::settings::general_value(&settings.general),
        "keybindings": settings.keyboard,
        "defaultKeybindings": [],
        "keybindingOverrides": [],
        "experiments": crate::settings::experiments_value(&settings.experiments),
        "appearance": crate::settings::appearance_value(&settings.appearance),
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

/// Loom-native workspace providers exposed to the product composer.
///
/// These are capability descriptors, not plugin registrations. Personal
/// workspaces use the worker's existing managed-environment provisioner;
/// project checkout uses an explicit local-path source on the selected host.
/// Per-machine availability is authoritative, so a remembered disconnected
/// host cannot silently fall back to another machine at submission time.
async fn environment_providers(
    State(state): State<AppState>,
    Query(query): Query<ProviderQuery>,
) -> Json<Value> {
    let project = query
        .project_id
        .as_deref()
        .and_then(|raw| raw.parse::<ProjectId>().ok())
        .and_then(|project_id| state.registry.project(&project_id));
    let hosts = state.registry.hosts();

    let mut personal_machines = serde_json::Map::new();
    let mut checkout_machines = serde_json::Map::new();
    for host in &hosts {
        if query
            .host_id
            .as_deref()
            .is_some_and(|requested| requested != host.id.to_string())
        {
            continue;
        }
        personal_machines.insert(
            host.id.to_string(),
            environment_provider_machine_availability(host, true),
        );
        let has_source = project.as_ref().is_some_and(|project| {
            project
                .sources
                .iter()
                .any(|source| source.host_id == host.id && !source.path.is_empty())
        });
        checkout_machines.insert(
            host.id.to_string(),
            environment_provider_machine_availability(host, has_source),
        );
    }

    let personal_availability = aggregate_provider_availability(&personal_machines);
    let checkout_availability = aggregate_provider_availability(&checkout_machines);
    Json(json!({
        "providers": [
            {
                "id": PERSONAL_WORKSPACE_PROVIDER_ID,
                "displayName": "Personal workspace",
                "icon": "Folder",
                "logoUrl": null,
                "pluginId": "environment-personal-workspace",
                "requires": {
                    "projectCheckout": false,
                    "gitCheckout": false,
                    "gitRemote": false,
                    "projectless": true
                },
                "inputs": null,
                "acceptsEmptyInputs": true,
                "availability": personal_availability,
                "machineAvailability": personal_machines
            },
            {
                "id": PROJECT_CHECKOUT_PROVIDER_ID,
                "displayName": "Project checkout",
                "icon": "Laptop",
                "logoUrl": null,
                "pluginId": "environment-project-checkout",
                "requires": {
                    "projectCheckout": true,
                    "gitCheckout": false,
                    "gitRemote": false,
                    "projectless": false
                },
                "inputs": null,
                "acceptsEmptyInputs": true,
                "availability": checkout_availability,
                "machineAvailability": checkout_machines
            }
        ]
    }))
}

async fn execution_options(
    State(state): State<AppState>,
    Query(query): Query<ProviderQuery>,
) -> Json<Value> {
    let provider = query_provider(&state, &query);
    let catalog = catalog_for_query(&state, &provider.name, &query);
    Json(execution_options_for(&state, &provider, catalog.as_ref()))
}

async fn system_providers(
    State(state): State<AppState>,
    Query(_query): Query<ProviderQuery>,
) -> Json<Value> {
    Json(json!(state
        .providers()
        .iter()
        .map(provider_info)
        .collect::<Vec<_>>()))
}

async fn system_provider_states(
    State(state): State<AppState>,
    Query(_query): Query<ProviderQuery>,
) -> Json<Value> {
    // One entry per configured agent: the client uses this list to find a
    // provider it can use, so reporting only the default would hide the others.
    let providers: Vec<Value> = state
        .providers()
        .iter()
        .map(|spec| {
            json!({
                "status": "unknown",
                "statusMessage": null,
                "accountEmail": null,
                "planLabel": null,
                "installedVersion": null,
                "minimumSupportedVersion": null,
                "canInstall": false,
                "canUpdate": false,
                "loginCommand": null,
                "providerId": spec.name,
                "displayName": provider_display_name(&spec.name)
            })
        })
        .collect();
    Json(json!({ "providers": providers }))
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

pub(crate) fn project_source_value(source: &loom_domain::ProjectSource) -> Value {
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
pub(crate) fn project_value(project: &Project) -> Value {
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
pub(crate) fn environment_value(environment: &Environment) -> Value {
    let lifecycle_phase = if environment.status == EnvironmentStatus::Destroyed {
        "destroyed"
    } else {
        "active"
    };
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
        "mergeBaseBranch": environment.merge_base_branch,
        "status": environment.status,
        "environmentProviderId": null,
        "lifecycle": { "phase": lifecycle_phase, "retireAt": null, "teardown": null },
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
        "maxPermissionMode": host.max_permission_mode.as_str(),
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
        // The agent this thread runs on, which is the one its client picked;
        // a thread that never picked one ran on the default.
        "providerId": thread
            .provider_id
            .clone()
            .unwrap_or_else(|| configured_provider_id(state)),
        "title": thread.title,
        "titleFallback": thread.title,
        "sectionId": thread.section_id,
        "status": bb_thread_status(thread.status),
        "parentThreadId": thread.parent_thread_id.as_ref().map(ToString::to_string),
        "sourceThreadId": thread.source_thread_id.as_ref().map(ToString::to_string),
        "originKind": thread.origin_kind.map(|origin| origin.as_str()),
        "originPluginId": thread.origin_plugin_id,
        // A stored field, not a function of `status`: bb's archived and hidden
        // flags are independent, so archiving a thread does not hide it and
        // hiding one does not archive it.
        "visibility": thread.visibility.as_str(),
        "archivedAt": thread.archived_at_ms,
        "pinnedAt": thread.pinned_at_ms,
        "deletedAt": thread.deleted_at_ms,
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
        // A real count: a queued message is durable state, and the composer's
        // "queued" affordance is driven by this number. Reporting `0` while
        // the queue held rows is the kind of fake the batch forbids.
        "queuedMessageCount": state.registry.queued_message_count(&thread.id)
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
            "activePlanModeCount": usize::from(thread_has_active_plan(state, &thread.id)),
            "activeGoalCount": usize::from(thread_has_goal(state, &thread.id)),
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
    object.insert(
        "pinSortKey".into(),
        thread
            .pin_sort_key
            .clone()
            .map_or(Value::Null, Value::String),
    );
    object.insert(
        "hasPendingInteraction".into(),
        Value::Bool(state.registry.has_pending_interaction(&thread.id)),
    );
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

fn error_response_with_details(
    status: StatusCode,
    code: &'static str,
    message: String,
    details: Value,
) -> Response {
    (
        status,
        Json(json!({
            "code": code,
            "message": message,
            "details": details,
        })),
    )
        .into_response()
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
    /// The agent the client picked for this thread, when it picked one.
    #[serde(default)]
    pub provider_id: Option<String>,
    /// The model the client picked, as the agent's own id.
    #[serde(default)]
    pub model: Option<String>,
    /// The reasoning level the client picked.
    #[serde(default)]
    pub reasoning_level: Option<ReasoningLevel>,
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
pub(crate) fn thread_domain_events(
    state: &AppState,
    thread_id: &ThreadId,
) -> Result<Vec<(String, u64, u64, DomainEvent)>, Response> {
    let scope = Scope::Thread(thread_id.to_string());
    // The **retained** log, not the replay window: a timeline is history. The
    // window is for a reader that just attached, and reading history through it
    // hid every conversation whose thread had been quiet for five minutes.
    let envelopes = state
        .relay
        .retained_scope(&scope, usize::MAX)
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
    if let Err(response) = public_thread_or_response(&state, &thread_id) {
        return response;
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

    let rows = match thread_event_rows(&state, &thread_id) {
        Ok(rows) => rows,
        Err(response) => return response,
    };
    // The row set and its shape come from [`thread_event_rows`], which
    // `threads.eventWait` shares: the two routes cannot drift on what a
    // `ThreadEventRow` is.
    let mut result = rows
        .into_iter()
        .filter(|(sequence, row)| {
            !after.is_some_and(|value| *sequence <= value)
                && !before.is_some_and(|value| *sequence >= value)
                && types.as_ref().is_none_or(|allowed| {
                    allowed.contains(&row["type"].as_str().unwrap_or_default())
                })
        })
        .map(|(_, row)| row)
        .collect::<Vec<_>>();
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
    match public_thread_or_response(&state, &thread_id) {
        Ok(thread) => Json(thread_summary_value(&state, &thread)).into_response(),
        Err(response) => response,
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
    if let Err(response) = public_thread_or_response(&state, &thread_id) {
        return response;
    }

    let entries = match thread_domain_events(&state, &thread_id) {
        Ok(entries) => entries,
        Err(response) => return response,
    };
    // The same fold the timeline uses, so `threads.output` and the rows a client
    // renders cannot disagree about an assistant answer.
    let assistant_messages = assistant_message_timeline(&entries);
    let mut completed_output = None;
    for (_event_id, _sequence, _created_at_ms, event) in &entries {
        let DomainEvent::ThreadRunEvent { run } = event else {
            continue;
        };
        let value = serde_json::to_value(&run.event).expect("ThreadEvent always serializes");
        if value.get("type").and_then(Value::as_str) == Some("item/completed")
            && value
                .get("item")
                .and_then(|item| item.get("type"))
                .and_then(Value::as_str)
                == Some("agentMessage")
        {
            completed_output = value
                .get("item")
                .and_then(|item| item.get("text"))
                .and_then(Value::as_str)
                .map(str::to_owned);
        }
    }
    let output = if assistant_messages.is_empty() {
        completed_output
    } else {
        Some(assistant_messages.concatenated_text())
    };
    Json(json!({ "output": output })).into_response()
}

async fn read_thread(State(state): State<AppState>, Path(raw_thread_id): Path<String>) -> Response {
    let thread_id = match parse_thread_id(&raw_thread_id) {
        Ok(thread_id) => thread_id,
        Err(response) => return response,
    };
    if let Err(response) = public_thread_or_response(&state, &thread_id) {
        return response;
    }
    match state
        .registry
        .mark_thread_read(&thread_id, loom_relay::now_ms())
    {
        Ok((thread, event)) => {
            if let Some(event) = event {
                let _ = state.publish_domain_event(&event);
            }
            Json(thread_summary_value(&state, &thread)).into_response()
        }
        Err(error) => command_error_response(error),
    }
}

/// The lifecycle acknowledgement shared by archive, unarchive and delete.
fn lifecycle_ok() -> Response {
    Json(json!({ "ok": true })).into_response()
}

/// A deleted thread is retained for replay, but is no longer a client resource.
#[allow(clippy::result_large_err)]
fn public_thread_or_response(state: &AppState, thread_id: &ThreadId) -> Result<Thread, Response> {
    match state.registry.thread(thread_id) {
        Some(thread) if thread.deleted_at_ms.is_none() => Ok(thread),
        Some(_) => Err(error_response_with_code(
            StatusCode::NOT_FOUND,
            "thread_not_found",
            format!("thread {thread_id} has been deleted"),
        )),
        None => Err(error_response(
            StatusCode::NOT_FOUND,
            format!("thread {thread_id} is not known"),
        )),
    }
}

/// Archives one thread. Archiving is idempotent; the event is only published
/// when the status actually changes.
async fn archive_thread(
    State(state): State<AppState>,
    Path(raw_thread_id): Path<String>,
) -> Response {
    let thread_id = match parse_thread_id(&raw_thread_id) {
        Ok(thread_id) => thread_id,
        Err(response) => return response,
    };
    if let Err(response) = public_thread_or_response(&state, &thread_id) {
        return response;
    }
    match state
        .registry
        .archive_thread(&thread_id, loom_relay::now_ms())
    {
        Ok((_thread, event)) => {
            crate::b9::close_thread_terminals(
                &state,
                &thread_id,
                loom_provider_protocol::TerminalCloseReason::ThreadArchived,
            );
            if let Some(event) = event {
                if let Err(error) = state.publish_domain_event(&event) {
                    return error_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string());
                }
            }
            lifecycle_ok()
        }
        Err(error) => command_error_response(error),
    }
}

/// Archives a thread and its direct child/source-fork threads.
async fn archive_all_threads(
    State(state): State<AppState>,
    Path(raw_thread_id): Path<String>,
) -> Response {
    let thread_id = match parse_thread_id(&raw_thread_id) {
        Ok(thread_id) => thread_id,
        Err(response) => return response,
    };
    if let Err(response) = public_thread_or_response(&state, &thread_id) {
        return response;
    }
    match state
        .registry
        .archive_all_threads(&thread_id, loom_relay::now_ms())
    {
        Ok((archived_ids, events)) => {
            if let Err(error) = publish_all(&state, &events) {
                return error_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string());
            }
            Json(json!({
                "ok": true,
                "archivedThreadIds": archived_ids
                    .into_iter()
                    .map(|id| id.to_string())
                    .collect::<Vec<_>>(),
            }))
            .into_response()
        }
        Err(error) => command_error_response(error),
    }
}

/// Restores an archived thread to the domain's idle state.
async fn unarchive_thread(
    State(state): State<AppState>,
    Path(raw_thread_id): Path<String>,
) -> Response {
    let thread_id = match parse_thread_id(&raw_thread_id) {
        Ok(thread_id) => thread_id,
        Err(response) => return response,
    };
    if let Err(response) = public_thread_or_response(&state, &thread_id) {
        return response;
    }
    match state
        .registry
        .unarchive_thread(&thread_id, loom_relay::now_ms())
    {
        Ok((_thread, event)) => {
            if let Some(event) = event {
                if let Err(error) = state.publish_domain_event(&event) {
                    return error_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string());
                }
            }
            lifecycle_ok()
        }
        Err(error) => command_error_response(error),
    }
}

/// The request body for `threads.delete`.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeleteThreadRequest {
    child_threads_confirmed: bool,
}

/// Deletes a thread as a tombstone. Child threads require an explicit client
/// confirmation because their parent reference is part of the visible model.
async fn delete_thread(
    State(state): State<AppState>,
    Path(raw_thread_id): Path<String>,
    Json(request): Json<DeleteThreadRequest>,
) -> Response {
    let thread_id = match parse_thread_id(&raw_thread_id) {
        Ok(thread_id) => thread_id,
        Err(response) => return response,
    };
    if let Err(response) = public_thread_or_response(&state, &thread_id) {
        return response;
    }
    let child_count = state.registry.child_count(&thread_id);
    if child_count > 0 && !request.child_threads_confirmed {
        return error_response_with_details(
            StatusCode::CONFLICT,
            "child_threads_confirmation_required",
            format!("thread {thread_id} has {child_count} child thread(s); confirm deletion"),
            json!({ "childThreadCount": child_count }),
        );
    }
    match state
        .registry
        .delete_thread(&thread_id, loom_relay::now_ms())
    {
        Ok((_thread, event)) => {
            crate::b9::close_thread_terminals(
                &state,
                &thread_id,
                loom_provider_protocol::TerminalCloseReason::ThreadDeleted,
            );
            if let Some(event) = event {
                if let Err(error) = state.publish_domain_event(&event) {
                    return error_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string());
                }
            }
            lifecycle_ok()
        }
        Err(error) => command_error_response(error),
    }
}

/// The fork request is intentionally opaque after `sourceThreadId`: the
/// contract validates the input/environment seed, while loom cannot yet pass
/// either to ACP's session/fork capability.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ForkThreadRequest {
    source_thread_id: String,
    #[serde(flatten)]
    _options: std::collections::BTreeMap<String, Value>,
}

/// Forks a provider session when the execution plane supports it.
async fn fork_thread(
    State(state): State<AppState>,
    Json(request): Json<ForkThreadRequest>,
) -> Response {
    let source_thread_id = match parse_thread_id(&request.source_thread_id) {
        Ok(thread_id) => thread_id,
        Err(response) => return response,
    };
    let source = match public_thread_or_response(&state, &source_thread_id) {
        Ok(thread) => thread,
        Err(response) => return response,
    };
    if source.status == ThreadStatus::Archived {
        return error_response_with_details(
            StatusCode::CONFLICT,
            "thread_not_writable",
            format!("thread {source_thread_id} is archived and cannot be forked"),
            json!({
                "reason": "archived",
                "archivedAt": source.archived_at_ms,
                "threadStatus": bb_thread_status(source.status),
            }),
        );
    }
    if source.provider_session_id.is_none() {
        return error_response_with_code(
            StatusCode::BAD_REQUEST,
            "fork_source_session_unavailable",
            format!("thread {source_thread_id} has no provider session to use as a fork source"),
        );
    }
    error_response_with_code(
        StatusCode::NOT_IMPLEMENTED,
        "not_configured",
        format!("ACP session/fork is not configured for source thread {source_thread_id}"),
    )
}

/// Pins a thread at the front of the pinned order.
async fn pin_thread(State(state): State<AppState>, Path(raw_thread_id): Path<String>) -> Response {
    let thread_id = match parse_thread_id(&raw_thread_id) {
        Ok(thread_id) => thread_id,
        Err(response) => return response,
    };
    if let Err(response) = public_thread_or_response(&state, &thread_id) {
        return response;
    }
    match state.registry.pin_thread(&thread_id, loom_relay::now_ms()) {
        Ok((thread, events)) => {
            if let Err(error) = publish_all(&state, &events) {
                return error_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string());
            }
            Json(thread_summary_value(&state, &thread)).into_response()
        }
        Err(error) => command_error_response(error),
    }
}

/// Removes a thread from the pinned order.
async fn unpin_thread(
    State(state): State<AppState>,
    Path(raw_thread_id): Path<String>,
) -> Response {
    let thread_id = match parse_thread_id(&raw_thread_id) {
        Ok(thread_id) => thread_id,
        Err(response) => return response,
    };
    if let Err(response) = public_thread_or_response(&state, &thread_id) {
        return response;
    }
    match state
        .registry
        .unpin_thread(&thread_id, loom_relay::now_ms())
    {
        Ok((thread, event)) => {
            if let Some(event) = event {
                if let Err(error) = state.publish_domain_event(&event) {
                    return error_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string());
                }
            }
            Json(thread_summary_value(&state, &thread)).into_response()
        }
        Err(error) => command_error_response(error),
    }
}

/// Clears the read marker without touching timeline events.
async fn mark_thread_unread(
    State(state): State<AppState>,
    Path(raw_thread_id): Path<String>,
) -> Response {
    let thread_id = match parse_thread_id(&raw_thread_id) {
        Ok(thread_id) => thread_id,
        Err(response) => return response,
    };
    if let Err(response) = public_thread_or_response(&state, &thread_id) {
        return response;
    }
    match state
        .registry
        .mark_thread_unread(&thread_id, loom_relay::now_ms())
    {
        Ok((thread, event)) => {
            if let Some(event) = event {
                if let Err(error) = state.publish_domain_event(&event) {
                    return error_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string());
                }
            }
            Json(thread_summary_value(&state, &thread)).into_response()
        }
        Err(error) => command_error_response(error),
    }
}

/// Request body for `threads.pinOrder`.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReorderPinnedThreadRequest {
    previous_thread_id: Option<String>,
    next_thread_id: Option<String>,
}

#[allow(clippy::result_large_err)]
fn parse_optional_thread_id(raw: Option<&str>) -> Result<Option<ThreadId>, Response> {
    raw.filter(|value| !value.is_empty())
        .map(|value| {
            value
                .parse::<ThreadId>()
                .map_err(|error| error_response(StatusCode::BAD_REQUEST, error.to_string()))
        })
        .transpose()
}

/// Reorders one pinned thread between the requested neighbors.
async fn reorder_pinned_thread(
    State(state): State<AppState>,
    Path(raw_thread_id): Path<String>,
    Json(request): Json<ReorderPinnedThreadRequest>,
) -> Response {
    let thread_id = match parse_thread_id(&raw_thread_id) {
        Ok(thread_id) => thread_id,
        Err(response) => return response,
    };
    if let Err(response) = public_thread_or_response(&state, &thread_id) {
        return response;
    }
    let previous_thread_id = match parse_optional_thread_id(request.previous_thread_id.as_deref()) {
        Ok(id) => id,
        Err(response) => return response,
    };
    let next_thread_id = match parse_optional_thread_id(request.next_thread_id.as_deref()) {
        Ok(id) => id,
        Err(response) => return response,
    };
    match state.registry.reorder_pinned_thread(
        &thread_id,
        previous_thread_id.as_ref(),
        next_thread_id.as_ref(),
        loom_relay::now_ms(),
    ) {
        Ok((threads, events)) => {
            if let Err(error) = publish_all(&state, &events) {
                return error_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string());
            }
            Json(
                threads
                    .iter()
                    .map(|thread| thread_list_entry_value(&state, thread))
                    .collect::<Vec<_>>(),
            )
            .into_response()
        }
        Err(error) => command_error_response(error),
    }
}

/// Request body for `threads.resolveMentions`.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ResolveThreadMentionsRequest {
    thread_ids: Vec<String>,
}

/// Resolves live thread ids into the labels used by prompt mentions.
async fn resolve_thread_mentions(
    State(state): State<AppState>,
    Json(request): Json<ResolveThreadMentionsRequest>,
) -> Response {
    let mut thread_ids = Vec::with_capacity(request.thread_ids.len());
    for raw_thread_id in request.thread_ids {
        match raw_thread_id.parse::<ThreadId>() {
            Ok(thread_id) => thread_ids.push(thread_id),
            Err(error) => return error_response(StatusCode::BAD_REQUEST, error.to_string()),
        }
    }
    Json(
        state
            .registry
            .resolve_mention_threads(&thread_ids)
            .into_iter()
            .map(|thread| {
                json!({
                    "threadId": thread.id.to_string(),
                    "projectId": thread.project_id.to_string(),
                    "label": thread
                        .title
                        .unwrap_or_else(|| format!("Thread {}", thread.id)),
                })
            })
            .collect::<Vec<_>>(),
    )
    .into_response()
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
    /// When the client wants the turn to run; a future value becomes a queued
    /// message instead of a turn.
    #[serde(default)]
    send_at: Option<u64>,
    /// The model, reasoning level, permission mode and service tier the client
    /// picked. The model and level are recorded on the thread before the turn
    /// dispatches, because the dispatch reads the thread's record rather than
    /// this request; on a queued message they are also kept on the row, so the
    /// turn runs with the options it was created with.
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    reasoning_level: Option<ReasoningLevel>,
    #[serde(default)]
    permission_mode: Option<String>,
    #[serde(default)]
    service_tier: Option<String>,
    #[serde(default)]
    sender_thread_id: Option<String>,
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
/// the compatibility `/messages` endpoint, or queues it when the thread cannot
/// take a turn now.
///
/// The contract's `mode` is what decides which, and loom honours the
/// distinction rather than collapsing it:
///
/// | mode | thread idle | thread busy |
/// | --- | --- | --- |
/// | `start` | send | `501 not_configured` |
/// | `auto` | send | queue, answer `delivery: "queued"` |
/// | `queue-if-active` | send | queue, answer `delivery: "queued"` |
/// | `steer` | send | `501 not_configured` |
/// | `steer-if-active` | send | `501 not_configured` |
///
/// The two `steer` modes are refused while busy because steering means
/// injecting input into the running turn, and `loom_provider_protocol` has no
/// frame for that. Appending the text as a second concurrent turn would be a
/// different operation under the same name. A future `sendAt` is likewise a
/// queue entry, never an immediate turn.
async fn send_thread(
    State(state): State<AppState>,
    Path(raw_thread_id): Path<String>,
    Json(request): Json<SendThreadRequest>,
) -> Response {
    let thread_id = match parse_thread_id(&raw_thread_id) {
        Ok(thread_id) => thread_id,
        Err(response) => return response,
    };
    let thread = match public_thread_or_response(&state, &thread_id) {
        Ok(thread) => thread,
        Err(response) => return response,
    };
    let Some(content) = text_from_send_input(&request.input) else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "loom currently accepts text prompt inputs only".into(),
        );
    };

    let busy =
        !matches!(thread.status, ThreadStatus::Idle) || state.runs.for_thread(&thread_id).is_some();
    let scheduled = request.send_at.is_some_and(|at| at > loom_relay::now_ms());
    let steers = matches!(request.mode, SendMode::Steer | SendMode::SteerIfActive);
    let queues_when_busy = matches!(request.mode, SendMode::Auto | SendMode::QueueIfActive);

    if busy && steers {
        return error_response_with_code(
            StatusCode::NOT_IMPLEMENTED,
            "not_configured",
            format!(
                "loom's provider protocol cannot steer a running turn; thread {thread_id} is \
                 {}",
                thread.status
            ),
        );
    }
    if busy && !queues_when_busy {
        // `start` while busy is the one combination with no honest answer: the
        // mode asked for a turn now, and the protocol cannot run two.
        return error_response_with_code(
            StatusCode::NOT_IMPLEMENTED,
            "not_configured",
            format!(
                "thread {thread_id} is {}; a `{}` send needs the queue, which this client did not \
                 ask for",
                thread.status,
                match request.mode {
                    SendMode::Start => "start",
                    SendMode::Steer | SendMode::SteerIfActive => "steer",
                    SendMode::Auto | SendMode::QueueIfActive => unreachable!(),
                },
            ),
        );
    }

    if busy || scheduled {
        let service_tier = match request.service_tier.as_deref() {
            None | Some("default") => ServiceTier::Default,
            Some("fast") => ServiceTier::Fast,
            Some(other) => {
                return error_response_with_code(
                    StatusCode::BAD_REQUEST,
                    "invalid_request",
                    format!("serviceTier must be `fast` or `default`, got {other:?}"),
                )
            }
        };
        let sender_thread_id = match request.sender_thread_id.as_deref() {
            None | Some("") => None,
            Some(raw) => match raw.parse::<ThreadId>() {
                Ok(thread_id) => Some(thread_id),
                Err(error) => return error_response(StatusCode::BAD_REQUEST, error.to_string()),
            },
        };
        let now = loom_relay::now_ms();
        return match state.registry.create_queued_message(
            NewQueuedMessage {
                thread_id: thread_id.clone(),
                sender_thread_id,
                initiator: QueuedMessageInitiator::User,
                text: content,
                model: request.model,
                reasoning_level: request.reasoning_level,
                permission_mode: request.permission_mode,
                service_tier,
                group_with_next: false,
                send_at: request.send_at,
                payload: QueuedMessagePayload::Inline,
            },
            now,
        ) {
            Ok((message, event)) => {
                let _ = state.publish_domain_event(&event);
                let waiting_on = if scheduled {
                    json!({ "kind": "time" })
                } else {
                    json!({ "kind": "thread-busy" })
                };
                let mut row = queued_message_row(&state, &message, now);
                if let Value::Object(object) = &mut row {
                    object.insert("waitingOn".into(), waiting_on);
                }
                Json(json!({
                    "ok": true,
                    "delivery": "queued",
                    "queuedMessage": row,
                }))
                .into_response()
            }
            Err(error) => command_error_response(error),
        };
    }

    // The turn about to be dispatched reads the thread's record, so the options
    // this request carried are recorded first. A queued message keeps its own
    // on the row instead; that branch returned above.
    apply_execution_options(
        &state,
        &thread_id,
        None,
        request.model.as_deref(),
        request.reasoning_level.as_ref(),
    );
    match append_thread_message(&state, &thread_id, MessageRole::User, content) {
        Ok(_) => Json(json!({ "ok": true, "delivery": "sent" })).into_response(),
        Err(response) => response,
    }
}

/// How many threads a `threads.search` group returns when the client names no
/// limit.
const SEARCH_DEFAULT_LIMIT_PER_GROUP: usize = 20;
/// The hard cap on `limitPerGroup`, so one query cannot ask for every thread.
const SEARCH_MAX_LIMIT_PER_GROUP: u64 = 100;
/// How many matches a search reports for one thread before moving on. A result
/// row is a preview, not the whole conversation.
const SEARCH_MAX_MATCHES_PER_THREAD: usize = 5;
/// How much of a matched text a search result carries, in characters.
const SEARCH_SNIPPET_CHARS: usize = 200;
/// How much of a message a conversation-outline preview carries, in characters.
const OUTLINE_PREVIEW_CHARS: usize = 120;
/// How many prompts a `threads.promptHistory` read returns by default.
const PROMPT_HISTORY_DEFAULT_LIMIT: usize = 50;
/// The hard cap on that read.
const PROMPT_HISTORY_MAX_LIMIT: u64 = 200;
/// The reasoning level loom runs a provider at when a thread names no other.
const DEFAULT_REASONING_LEVEL: &str = "medium";
/// The permission mode loom runs a provider under, and the ceiling every host
/// advertises.
const DEFAULT_PERMISSION_MODE: &str = "full";
/// The service tier loom reports: its provider protocol has no fast tier.
const DEFAULT_SERVICE_TIER: &str = "default";

/// The thread's tabs (`threads.tabs`) and the revision a write must name.
///
/// The revision is a compare-and-swap token, not a timestamp: it starts at `0`
/// and every accepted `PUT` increments it, so two clients editing the same
/// thread cannot silently overwrite each other.
async fn thread_tabs(State(state): State<AppState>, Path(raw_thread_id): Path<String>) -> Response {
    let thread_id = match parse_thread_id(&raw_thread_id) {
        Ok(thread_id) => thread_id,
        Err(response) => return response,
    };
    match public_thread_or_response(&state, &thread_id) {
        Ok(thread) => Json(json!({
            "revision": thread.tabs_revision,
            "tabs": thread.tabs,
        }))
        .into_response(),
        Err(response) => response,
    }
}

/// Body of a `threads.updateTabs` request.
///
/// The tab objects are kept as JSON: the shape is bb's `tabsSchema`, which has
/// ten variants and grows with the client, and loom renders none of them. The
/// contract middleware validates every write against that schema before the
/// handler sees it, so an opaque round-trip cannot store a tab bb would reject.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpdateThreadTabsRequest {
    expected_revision: u64,
    tabs: Vec<Value>,
}

/// Replaces a thread's tabs, refusing a write that named a stale revision.
///
/// A revision mismatch is bb's `thread_tabs_conflict` at `409`: the write was
/// well-formed, the client's view was not current, and re-reading the tabs is
/// what fixes it. Answering `200` with someone else's tabs would be the lost
/// update the revision exists to prevent.
async fn update_thread_tabs(
    State(state): State<AppState>,
    Path(raw_thread_id): Path<String>,
    Json(request): Json<UpdateThreadTabsRequest>,
) -> Response {
    let thread_id = match parse_thread_id(&raw_thread_id) {
        Ok(thread_id) => thread_id,
        Err(response) => return response,
    };
    if let Err(response) = public_thread_or_response(&state, &thread_id) {
        return response;
    }
    match state.registry.set_thread_tabs(
        &thread_id,
        request.tabs,
        request.expected_revision,
        loom_relay::now_ms(),
    ) {
        Ok((thread, event)) => match state.publish_domain_event(&event) {
            Ok(_) => Json(json!({
                "revision": thread.tabs_revision,
                "tabs": thread.tabs,
            }))
            .into_response(),
            Err(error) => error_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
        },
        Err(CommandError::Conflict(message)) => {
            error_response_with_code(StatusCode::CONFLICT, "thread_tabs_conflict", message)
        }
        Err(error) => command_error_response(error),
    }
}

/// Records the execution options a request carried onto a thread.
///
/// A client picks the provider, model and reasoning level in the composer and
/// sends them with the thread it creates or the turn it starts. The run is
/// dispatched from the *thread's* record, so without this the choice would be
/// dropped and every run would use the server's defaults. Fields the request
/// left out are left alone, so a caller that names only a model keeps the
/// provider the thread already had.
fn apply_execution_options(
    state: &AppState,
    thread_id: &ThreadId,
    provider_id: Option<&str>,
    model: Option<&str>,
    reasoning_level: Option<&ReasoningLevel>,
) {
    if provider_id.is_none() && model.is_none() && reasoning_level.is_none() {
        return;
    }
    let update = ThreadUpdate {
        provider_id: provider_id.map(|value| Some(value.to_owned())),
        model: model.map(|value| Some(value.to_owned())),
        reasoning_level: reasoning_level.map(|level| Some(level.clone())),
        ..ThreadUpdate::default()
    };
    match state
        .registry
        .update_thread(thread_id, &update, loom_relay::now_ms())
    {
        Ok((_, Some(event))) => {
            let _ = state.publish_domain_event(&event);
        }
        Ok((_, None)) => {}
        Err(error) => {
            eprintln!(
                "loom-server: could not record execution options on thread {thread_id}: {error}"
            );
        }
    }
}

/// Applies `threads.update`: title, section, parent, visibility and the
/// thread's execution options.
///
/// The response is the whole thread (`threadSchema`), not an acknowledgement,
/// because the client's next render is the row it just changed. A body that
/// changes nothing is accepted and answers the unchanged thread: every field in
/// the contract's request is optional, so "no change" cannot be an error.
///
/// `model`, `reasoningLevel` and `providerId` are recorded and reported
/// (`threads.defaultExecutionOptions`), and all three are carried into the next
/// dispatch: the worker hands the model and level to the agent, and the
/// provider picks the agent itself. An id the server does not configure falls
/// back to the default provider rather than failing the turn.
async fn update_thread(
    State(state): State<AppState>,
    Path(raw_thread_id): Path<String>,
    Json(update): Json<ThreadUpdate>,
) -> Response {
    let thread_id = match parse_thread_id(&raw_thread_id) {
        Ok(thread_id) => thread_id,
        Err(response) => return response,
    };
    if let Err(response) = public_thread_or_response(&state, &thread_id) {
        return response;
    }
    match state
        .registry
        .update_thread(&thread_id, &update, loom_relay::now_ms())
    {
        Ok((thread, event)) => {
            if let Some(event) = event {
                if let Err(error) = state.publish_domain_event(&event) {
                    return error_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string());
                }
            }
            Json(thread_summary_value(&state, &thread)).into_response()
        }
        Err(error) => command_error_response(error),
    }
}

/// `threads.childSummary`: how many threads were delegated from this one.
///
/// The contract counts *non-deleted* children; loom has no thread deletion, so
/// every child counts until one exists.
async fn thread_child_summary(
    State(state): State<AppState>,
    Path(raw_thread_id): Path<String>,
) -> Response {
    let thread_id = match parse_thread_id(&raw_thread_id) {
        Ok(thread_id) => thread_id,
        Err(response) => return response,
    };
    if let Err(response) = public_thread_or_response(&state, &thread_id) {
        return response;
    }
    Json(json!({ "nonDeletedChildCount": state.registry.child_count(&thread_id) })).into_response()
}

/// `threads.running`: the threads with a provider run in flight.
///
/// Read from the run table, which is the authority on what is executing — a
/// thread's `working` status is the *intent* to run, and a run that failed to
/// dispatch has already been reconciled out of it.
async fn running_threads(State(state): State<AppState>) -> Json<Vec<Value>> {
    let mut in_flight: std::collections::BTreeMap<ThreadId, HostId> =
        std::collections::BTreeMap::new();
    for run in state.runs.all() {
        in_flight
            .entry(run.thread_id.clone())
            .or_insert(run.host_id.clone());
    }
    Json(
        in_flight
            .into_iter()
            .map(|(thread_id, host_id)| {
                json!({ "id": thread_id.to_string(), "hostId": host_id.to_string() })
            })
            .collect(),
    )
}

/// The thread's effective execution options for its next run, or `null`.
///
/// `null` is the contract's second branch and loom's honest answer when the
/// thread records nothing: the effective level would be the *client's*
/// preference, which only the client knows. Once a client has written options
/// with `threads.update`, they are reported here as
/// `client/thread/start` — which is what they are.
///
/// `serviceTier` and `permissionMode` are loom's fixed values rather than
/// stored ones: the provider protocol has no service tier, and every host
/// advertises `full` as its permission ceiling.
async fn thread_default_execution_options(
    State(state): State<AppState>,
    Path(raw_thread_id): Path<String>,
) -> Response {
    let thread_id = match parse_thread_id(&raw_thread_id) {
        Ok(thread_id) => thread_id,
        Err(response) => return response,
    };
    let thread = match public_thread_or_response(&state, &thread_id) {
        Ok(thread) => thread,
        Err(response) => return response,
    };
    if thread.model.is_none() && thread.reasoning_level.is_none() {
        return Json(Value::Null).into_response();
    }
    // `providerId` is deliberately absent: this route's declared response is
    // `additionalProperties: false` and does not carry it. The agent a thread is
    // bound to is reported on the thread summary, which is where the client
    // reads it.
    Json(json!({
        "model": thread
            .model
            .clone()
            .unwrap_or_else(|| configured_provider_id(&state)),
        "serviceTier": DEFAULT_SERVICE_TIER,
        "reasoningLevel": thread.reasoning_level.clone().map_or_else(
            || DEFAULT_REASONING_LEVEL.to_owned(),
            |level| level.to_string(),
        ),
        "permissionMode": DEFAULT_PERMISSION_MODE,
        "source": "client/thread/start",
    }))
    .into_response()
}

/// The options a thread created in this project would start with.
///
/// Loom has no per-project overrides, so this is the server's configured
/// provider and model — the same values `system.execution-options` reports —
/// rather than a `null` that would say "unknowable". If no provider is
/// configured at all, there is nothing to report and the contract's `null`
/// branch is the answer.
async fn project_default_execution_options(
    State(state): State<AppState>,
    Path(raw_project_id): Path<String>,
) -> Response {
    let Ok(project_id) = raw_project_id.parse::<ProjectId>() else {
        return error_response(
            StatusCode::BAD_REQUEST,
            format!("{raw_project_id:?} is not a project id"),
        );
    };
    if state.registry.project(&project_id).is_none() {
        return error_response(
            StatusCode::NOT_FOUND,
            format!("project {project_id} is not known"),
        );
    }
    let provider_id = configured_provider_id(&state);
    if provider_id.is_empty() {
        return Json(Value::Null).into_response();
    }
    // The configured model's `model` field is the provider id today; reading it
    // from `configured_model` keeps the two in step if that changes.
    let model = configured_model(&provider_id)
        .get("model")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| provider_id.clone());
    Json(json!({
        "providerId": provider_id,
        "model": model,
        "serviceTier": DEFAULT_SERVICE_TIER,
        "reasoningLevel": DEFAULT_REASONING_LEVEL,
        "permissionMode": DEFAULT_PERMISSION_MODE,
    }))
    .into_response()
}

/// `threads.conversationOutline`: one row per user and assistant message.
///
/// Projected from the relay log, which is where messages live — the registry
/// deliberately holds no timeline. `maxSeq` is the last sequence in the thread,
/// so a client can tell whether its outline is current.
async fn thread_conversation_outline(
    State(state): State<AppState>,
    Path(raw_thread_id): Path<String>,
) -> Response {
    let thread_id = match parse_thread_id(&raw_thread_id) {
        Ok(thread_id) => thread_id,
        Err(response) => return response,
    };
    if let Err(response) = public_thread_or_response(&state, &thread_id) {
        return response;
    }
    let entries = match thread_domain_events(&state, &thread_id) {
        Ok(entries) => entries,
        Err(response) => return response,
    };
    let mut items = Vec::new();
    for (_event_id, _sequence, _created_at_ms, event) in &entries {
        let DomainEvent::ThreadMessageAdded { message, .. } = event else {
            continue;
        };
        let role = match message.role {
            MessageRole::User => "user",
            MessageRole::Assistant => "assistant",
            MessageRole::System => continue,
        };
        items.push(json!({
            "id": message.id.to_string(),
            "role": role,
            "preview": preview_text(&message.content, OUTLINE_PREVIEW_CHARS),
            // Loom's messages carry no attachments yet; a null summary is the
            // contract's way of saying so, not a placeholder for one.
            "attachmentSummary": Value::Null,
        }));
    }
    let max_seq = entries
        .last()
        .map(|(_, sequence, _, _)| *sequence)
        .unwrap_or(0);
    Json(json!({ "items": items, "maxSeq": max_seq })).into_response()
}

/// Query fields of a `threads.promptHistory` read.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PromptHistoryQuery {
    limit: Option<String>,
}

/// `threads.promptHistory`: the thread's user prompts, newest first.
///
/// Newest first because that is the order a prompt-recall affordance walks, and
/// `limit` is what bounds it. The contract answers a bare array, not an
/// envelope. Mentions are always empty: loom stores a prompt's text and does
/// not resolve `@` references yet, so reporting none is the truth rather than a
/// dropped field.
async fn thread_prompt_history(
    State(state): State<AppState>,
    Path(raw_thread_id): Path<String>,
    Query(query): Query<PromptHistoryQuery>,
) -> Response {
    let thread_id = match parse_thread_id(&raw_thread_id) {
        Ok(thread_id) => thread_id,
        Err(response) => return response,
    };
    if let Err(response) = public_thread_or_response(&state, &thread_id) {
        return response;
    }
    let limit = match parse_query_sequence(query.limit.as_ref(), "limit") {
        Ok(Some(limit)) => limit.clamp(1, PROMPT_HISTORY_MAX_LIMIT) as usize,
        Ok(None) => PROMPT_HISTORY_DEFAULT_LIMIT,
        Err(response) => return response,
    };
    let entries = match thread_domain_events(&state, &thread_id) {
        Ok(entries) => entries,
        Err(response) => return response,
    };
    let mut prompts = entries
        .iter()
        .filter_map(
            |(_event_id, _sequence, _created_at_ms, event)| match event {
                DomainEvent::ThreadMessageAdded { message, .. }
                    if message.role == MessageRole::User =>
                {
                    Some(json!({
                        "id": message.id.to_string(),
                        "createdAt": message.created_at_ms,
                        "input": [{
                            "type": "text",
                            "text": message.content,
                            "mentions": [],
                        }],
                    }))
                }
                _ => None,
            },
        )
        .collect::<Vec<_>>();
    prompts.reverse();
    prompts.truncate(limit);
    Json(prompts).into_response()
}

/// Query fields of a `threads.search` read.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SearchQuery {
    query: String,
    limit_per_group: Option<String>,
}

/// `threads.search`: a case-insensitive substring search over titles and
/// messages, split into active and archived groups.
///
/// There is no index: the control plane walks the threads it holds and replays
/// each thread's room, which is why results are also capped. `total` is the
/// number of matching threads in the group *before* `limitPerGroup`, so a
/// client can show "20 of 43".
///
/// A `title_fallback` match is never produced: loom's `titleFallback` is the
/// title itself, so the text a client displays is already covered by `title`.
async fn search_threads(
    State(state): State<AppState>,
    Query(query): Query<SearchQuery>,
) -> Response {
    if query.query.chars().count() < 2 {
        // `400` rather than the middleware's `422`: the contract declares
        // `invalid_request` at 400/403/404/409/413, and a query the server
        // itself rejects should use the status its own code declares.
        return error_response_with_code(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "query must be at least 2 characters".to_owned(),
        );
    }
    let limit = match parse_query_sequence(query.limit_per_group.as_ref(), "limitPerGroup") {
        Ok(Some(limit)) => limit.clamp(1, SEARCH_MAX_LIMIT_PER_GROUP) as usize,
        Ok(None) => SEARCH_DEFAULT_LIMIT_PER_GROUP,
        Err(response) => return response,
    };
    let needle = query.query.to_lowercase();

    // Two groups, filled in one pass: a group keeps every matching thread's
    // count but only `limit` rows, which is what makes `total` useful.
    let mut active = Vec::new();
    let mut active_total = 0usize;
    let mut archived = Vec::new();
    let mut archived_total = 0usize;
    for thread in state.registry.threads() {
        let matches = match thread_search_matches(&state, &thread, &needle) {
            Ok(matches) => matches,
            Err(response) => return response,
        };
        if matches.is_empty() {
            continue;
        }
        let (rows, total) = if thread.status == ThreadStatus::Archived {
            (&mut archived, &mut archived_total)
        } else {
            (&mut active, &mut active_total)
        };
        *total += 1;
        if rows.len() < limit {
            rows.push(json!({
                "thread": thread_list_entry_value(&state, &thread),
                "matches": matches,
            }));
        }
    }
    Json(json!({
        "active": { "results": active, "total": active_total },
        "archived": { "results": archived, "total": archived_total },
    }))
    .into_response()
}

/// Everything a search found in one thread: the title first, then each message
/// that matches, in log order.
#[allow(clippy::result_large_err)]
fn thread_search_matches(
    state: &AppState,
    thread: &Thread,
    needle_lower: &str,
) -> Result<Vec<Value>, Response> {
    let mut matches = Vec::new();
    if let Some(title) = &thread.title {
        let occurrences = find_occurrences(title, needle_lower);
        if !occurrences.is_empty() {
            let (text, highlight_ranges) =
                snippet_with_highlights(title, &occurrences, SEARCH_SNIPPET_CHARS);
            matches.push(json!({
                "sourceKind": "title",
                "text": text,
                "highlightRanges": highlight_ranges,
                "sourceSeq": Value::Null,
            }));
        }
    }
    for (_event_id, sequence, _created_at_ms, event) in thread_domain_events(state, &thread.id)? {
        if matches.len() >= SEARCH_MAX_MATCHES_PER_THREAD {
            break;
        }
        let DomainEvent::ThreadMessageAdded { message, .. } = event else {
            continue;
        };
        let occurrences = find_occurrences(&message.content, needle_lower);
        if occurrences.is_empty() {
            continue;
        }
        let (text, highlight_ranges) =
            snippet_with_highlights(&message.content, &occurrences, SEARCH_SNIPPET_CHARS);
        matches.push(json!({
            "sourceKind": match message.role {
                MessageRole::User => "user_message",
                MessageRole::Assistant => "assistant_message",
                MessageRole::System => "system_message",
            },
            "text": text,
            "highlightRanges": highlight_ranges,
            "sourceSeq": sequence,
        }));
    }
    Ok(matches)
}

/// Character ranges of every case-insensitive occurrence of `needle_lower`
/// (already lowercased) in `text`.
///
/// Character indices, not bytes: the caller slices by char to build a snippet,
/// and bytes would panic in the middle of a multi-byte character.
fn find_occurrences(text: &str, needle_lower: &str) -> Vec<(usize, usize)> {
    let haystack: Vec<char> = text.chars().collect();
    let needle: Vec<char> = needle_lower.chars().collect();
    if needle.is_empty() {
        return Vec::new();
    }
    let mut found = Vec::new();
    let mut index = 0;
    while index + needle.len() <= haystack.len() {
        let hit =
            (0..needle.len()).all(|offset| lowercase_eq(haystack[index + offset], needle[offset]));
        if hit {
            found.push((index, index + needle.len()));
            index += needle.len();
        } else {
            index += 1;
        }
    }
    found
}

/// Whether `value` lowercases to exactly `lower`, a single character.
fn lowercase_eq(value: char, lower: char) -> bool {
    let mut folded = value.to_lowercase();
    folded.next() == Some(lower) && folded.next().is_none()
}

/// A window of `text` around its first match, with the highlight ranges of
/// every match that stays inside it.
///
/// The ranges are UTF-16 code units, like the client-side JavaScript that
/// renders them; a byte range would highlight the wrong span for any message
/// containing a non-ASCII character. The window is only applied to text longer
/// than `limit`, so a short message is returned whole.
fn snippet_with_highlights(
    text: &str,
    occurrences: &[(usize, usize)],
    limit: usize,
) -> (String, Vec<Value>) {
    let chars: Vec<char> = text.chars().collect();
    let (first_start, first_end) = occurrences[0];
    let (window_start, window_end) = if chars.len() <= limit {
        (0, chars.len())
    } else {
        let match_len = first_end - first_start;
        let before = limit.saturating_sub(match_len) / 2;
        let start = first_start.saturating_sub(before);
        let end = (start + limit).min(chars.len());
        (end.saturating_sub(limit), end)
    };
    let window = &chars[window_start..window_end];
    let snippet: String = window.iter().collect();

    // Prefix sums in UTF-16 code units, so a char index in the window becomes
    // the offset the contract's client measures in.
    let mut offsets = Vec::with_capacity(window.len() + 1);
    let mut offset = 0usize;
    for character in window {
        offsets.push(offset);
        offset += character.len_utf16();
    }
    offsets.push(offset);

    let highlights = occurrences
        .iter()
        .filter(|(start, end)| *start >= window_start && *end <= window_end)
        .map(|(start, end)| {
            json!({
                "start": offsets[start - window_start],
                "end": offsets[end - window_start],
            })
        })
        .collect();
    (snippet, highlights)
}

/// A one-line preview of a message: whitespace collapsed, then truncated.
fn preview_text(text: &str, limit: usize) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= limit {
        return collapsed;
    }
    let mut preview: String = collapsed.chars().take(limit).collect();
    preview.push('…');
    preview
}

/// Body of a `threads.open` request.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct OpenThreadFile {
    source: String,
    path: String,
    line_number: Option<u64>,
}

/// Body of a `threads.open` request: the file to open, or `null` to open the
/// pane without one, and where the split should go.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct OpenThreadRequest {
    file: Option<OpenThreadFile>,
    split: Option<String>,
}

/// `threads.open`: tells every client viewing this thread to open a file.
///
/// The request travels the only path the control plane has — into the thread's
/// relay room — and `delivered` reports how many local subscribers that room
/// currently has. It is therefore a fan-out count, not an acknowledgement: a
/// client with no subscriber gets `0` and learns about the open request by
/// replaying the room, exactly like every other frame.
///
/// The cost of that choice is that an open request is retained, so replaying a
/// thread re-delivers it. Loom has no ephemeral frame path by design; an open
/// request is idempotent for the client that receives it twice.
async fn open_thread(
    State(state): State<AppState>,
    Path(raw_thread_id): Path<String>,
    Json(request): Json<OpenThreadRequest>,
) -> Response {
    let thread_id = match parse_thread_id(&raw_thread_id) {
        Ok(thread_id) => thread_id,
        Err(response) => return response,
    };
    if let Err(response) = public_thread_or_response(&state, &thread_id) {
        return response;
    }
    let scope = Scope::Thread(thread_id.to_string());
    let frame = json!({
        "type": "thread_open_requested",
        "threadId": thread_id.to_string(),
        "file": request.file,
        "split": request.split,
        "atMs": loom_relay::now_ms(),
    });
    let payload = serde_json::to_vec(&frame).expect("an open request always serializes");
    if let Err(error) = state.publish(scope.clone(), payload) {
        return error_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string());
    }
    match state.hub.subscriber_count(scope).await {
        Ok(delivered) => Json(json!({ "delivered": delivered })).into_response(),
        Err(error) => error_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    }
}

/// `threads.compact`: refused, explicitly.
///
/// Compaction is the provider summarising its own context, and loom's provider
/// protocol cannot ask for it. `loom_provider_protocol` defines dispatch,
/// provision and report and nothing else, and the worker only ever *observes*
/// compaction: Pi decides to compact and the bridge maps its `compaction_end`
/// to `thread/compacted` (`crates/worker/src/provider.rs`,
/// `docs/event-model.md` row 7). There is no frame in either direction that
/// requests one.
///
/// Answering `{ "ok": true }` for a compaction that never happened is the
/// failure mode the acceptance criteria name, so the route answers bb's
/// `not_configured` at the `501` that code declares. It becomes implementable
/// the day the protocol grows a request frame — and the report path that would
/// carry the result already exists.
async fn compact_thread(
    State(state): State<AppState>,
    Path(raw_thread_id): Path<String>,
) -> Response {
    let thread_id = match parse_thread_id(&raw_thread_id) {
        Ok(thread_id) => thread_id,
        Err(response) => return response,
    };
    if let Err(response) = public_thread_or_response(&state, &thread_id) {
        return response;
    }
    error_response_with_code(
        StatusCode::NOT_IMPLEMENTED,
        "not_configured",
        format!(
            "loom's provider protocol has no compaction frame; thread {thread_id} cannot be \
             compacted"
        ),
    )
}

/// `threads.editMessage`: refused, explicitly.
///
/// Editing a sent message means rewriting a turn the provider already executed:
/// the conversation this server owns is an append-only log, and the provider
/// protocol has no rewind frame (bb's own provider capabilities report
/// `supportsSessionRewind: false`). An edit that appended a second message and
/// left the first in place would be a different conversation, not an edit, so
/// the route refuses with bb's `not_configured` at `501` instead of pretending.
///
/// The request body is still validated against the contract by the middleware,
/// which is what keeps the refusal a statement about loom and not about the
/// request.
async fn edit_thread_message(
    State(state): State<AppState>,
    Path(raw_thread_id): Path<String>,
) -> Response {
    let thread_id = match parse_thread_id(&raw_thread_id) {
        Ok(thread_id) => thread_id,
        Err(response) => return response,
    };
    if let Err(response) = public_thread_or_response(&state, &thread_id) {
        return response;
    }
    error_response_with_code(
        StatusCode::NOT_IMPLEMENTED,
        "not_configured",
        format!(
            "loom's provider protocol cannot rewrite a sent turn; message editing is not \
             available for thread {thread_id}"
        ),
    )
}

/// Body of a `threads.retry` request.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RetryThreadRequest {
    /// A human label for the retry, kept on a queued retry's payload so a
    /// client can show why a repeat was asked for.
    #[serde(default)]
    reason: Option<String>,
    /// When the client wants the retry to run; `null` or a past time means now.
    #[serde(default)]
    send_at: Option<u64>,
    /// The client's own turn-request id, echoed back so its optimistic row can
    /// be matched. Loom does not deduplicate retries by it.
    #[serde(default)]
    turn_request_id: Option<String>,
}

/// `threads.retry`: dispatches the thread's last user prompt again, or queues
/// the repeat when the thread cannot run it now.
///
/// This is deliberately the *existing* lifecycle, not a second state machine:
/// the thread moves to `working` through [`ThreadTrigger`] (`run_started` from
/// `idle`, `retry` from `error`) and the run goes out through
/// [`AppState::dispatch_thread`], so reconciliation, deadlines and terminal
/// events apply exactly as they do to a first attempt.
///
/// A retry the thread cannot take now becomes a **queued message** instead of a
/// refusal, which is the contract's second response branch and the completion
/// of what B2 deferred:
///
/// * a future `sendAt` — the client asked for a scheduled repeat;
/// * a run in flight — the client asked for the repeat after the current turn.
///
/// Both answer `delivery: "queued"` with the row and the reason it is waiting,
/// so a client can render it exactly like an inline queued message. Running it
/// immediately would ignore the schedule, and answering `sent` would claim a
/// turn that did not start.
///
/// Remaining refusals, each for a state the client can see:
///
/// * no user turn in the thread — `409 no_failed_turn`;
/// * an archived thread — `409 thread_not_writable`;
/// * a dispatch that found no usable environment or host — the same error the
///   run lifecycle would report, with the terminal event already published to
///   the thread.
///
/// `attempt` counts the retries the thread's log already shows, so the first
/// retry of a turn is `1`.
async fn retry_thread(
    State(state): State<AppState>,
    Path(raw_thread_id): Path<String>,
    Json(request): Json<RetryThreadRequest>,
) -> Response {
    let thread_id = match parse_thread_id(&raw_thread_id) {
        Ok(thread_id) => thread_id,
        Err(response) => return response,
    };
    let thread = match public_thread_or_response(&state, &thread_id) {
        Ok(thread) => thread,
        Err(response) => return response,
    };
    if thread.status.is_archived() {
        return error_response_with_code(
            StatusCode::CONFLICT,
            "thread_not_writable",
            format!("thread {thread_id} is archived"),
        );
    }

    // The contract's validator does not enforce `pattern`, so a client-supplied
    // id is checked here, before anything changes: echoing one back outside
    // bb's `creq_` shape would put a value in the response the client cannot
    // correlate with its own row.
    let turn_request_id = request
        .turn_request_id
        .clone()
        .unwrap_or_else(loom_domain::id::mint_turn_request_id);
    if !loom_domain::is_turn_request_id(&turn_request_id) {
        return error_response_with_code(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            format!("turnRequestId {turn_request_id:?} is not a creq_ id"),
        );
    }

    // The prompt comes from the thread's own log: it is the only record of what
    // the turn was, and a retry has to repeat that turn.
    let entries = match thread_domain_events(&state, &thread_id) {
        Ok(entries) => entries,
        Err(response) => return response,
    };
    let mut prompt = None;
    let mut runs_started = 0u64;
    for (_event_id, _sequence, _created_at_ms, event) in &entries {
        match event {
            DomainEvent::ThreadMessageAdded { message, .. }
                if message.role == MessageRole::User =>
            {
                prompt = Some(message.content.clone());
            }
            // Every entry into `working` is one run of this thread, whoever
            // caused it. That is the honest count to derive `attempt` from: a
            // retry issued from `error` and one issued after a stop differ in
            // their trigger but not in what the client is asking for, and a
            // count that only watched `error -> working` reported the second
            // retry as the first.
            DomainEvent::ThreadStatusChanged {
                to: ThreadStatus::Working,
                ..
            } => runs_started += 1,
            _ => {}
        }
    }
    // A retry that was queued — and maybe delivered, cancelled, or still
    // waiting — is an attempt in its own right: it is what makes "this is the
    // third attempt" true for a client retrying a busy thread. The runs already
    // counted include the original turn, which is not a retry, hence the +1.
    let queued_retries = state
        .registry
        .queued_messages_for(Some(&thread_id))
        .into_iter()
        .filter(|message| matches!(message.payload, QueuedMessagePayload::Retry { .. }))
        .count() as u64;
    let attempt = runs_started.saturating_sub(1) + queued_retries + 1;
    let Some(prompt) = prompt else {
        return error_response_with_code(
            StatusCode::CONFLICT,
            "no_failed_turn",
            format!("thread {thread_id} has no user turn to retry"),
        );
    };

    let now = loom_relay::now_ms();
    let busy = matches!(thread.status, ThreadStatus::Working | ThreadStatus::Waiting)
        || state.runs.for_thread(&thread_id).is_some();
    let scheduled = request.send_at.is_some_and(|send_at| send_at > now);
    if busy || scheduled {
        let reason = request.reason.clone().unwrap_or_else(|| "Retry".to_owned());
        return match state.registry.create_queued_message(
            NewQueuedMessage {
                thread_id: thread_id.clone(),
                sender_thread_id: None,
                initiator: QueuedMessageInitiator::User,
                text: prompt,
                model: thread.model.clone(),
                reasoning_level: thread.reasoning_level,
                permission_mode: None,
                service_tier: ServiceTier::Default,
                group_with_next: false,
                send_at: request.send_at,
                payload: QueuedMessagePayload::Retry {
                    retry_of_turn_request_id: turn_request_id.clone(),
                    attempt,
                    reason,
                },
            },
            now,
        ) {
            Ok((message, event)) => {
                let _ = state.publish_domain_event(&event);
                let waiting_on = if scheduled {
                    json!({ "kind": "time" })
                } else {
                    json!({ "kind": "thread-busy" })
                };
                Json(json!({
                    "ok": true,
                    "delivery": "queued",
                    "turnRequestId": turn_request_id,
                    "attempt": attempt,
                    "queuedMessageId": message.id.to_string(),
                    "waitingOn": waiting_on,
                    "sendAt": message.send_at,
                }))
                .into_response()
            }
            Err(error) => command_error_response(error),
        };
    }

    let trigger = match thread.status {
        ThreadStatus::Idle => ThreadTrigger::RunStarted,
        ThreadStatus::Error => ThreadTrigger::Retry,
        // Archived, working and waiting were handled above.
        _ => ThreadTrigger::Retry,
    };
    match state.registry.transition_thread(&thread_id, trigger, now) {
        Ok(Some(event)) => {
            if let Err(error) = state.publish_domain_event(&event) {
                return error_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string());
            }
        }
        Ok(None) => {
            return error_response_with_code(
                StatusCode::CONFLICT,
                "conflict",
                format!("thread {thread_id} is not in a status a retry applies to"),
            )
        }
        Err(error) => return command_error_response(error),
    }

    let Some(thread) = state.registry.public_thread(&thread_id) else {
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("thread {thread_id} disappeared during its retry"),
        );
    };
    match state.dispatch_thread(&thread, &prompt) {
        crate::runs::DispatchOutcome::Dispatched(_) => Json(json!({
            "ok": true,
            "delivery": "sent",
            "turnRequestId": turn_request_id,
            "attempt": attempt,
        }))
        .into_response(),
        crate::runs::DispatchOutcome::NoEnvironment { .. } => error_response_with_code(
            StatusCode::CONFLICT,
            "thread_environment_unavailable",
            format!("thread {thread_id} has no environment that can run a provider"),
        ),
        crate::runs::DispatchOutcome::NoHost { .. } => error_response_with_code(
            StatusCode::BAD_GATEWAY,
            "host_unavailable",
            format!("no connected host owns thread {thread_id}'s workspace"),
        ),
        crate::runs::DispatchOutcome::AlreadyInFlight { run_id } => error_response_with_code(
            StatusCode::CONFLICT,
            "run_in_flight",
            format!("thread {thread_id} already has run {run_id} in flight"),
        ),
        crate::runs::DispatchOutcome::PublishFailed { error, .. } => {
            error_response(StatusCode::INTERNAL_SERVER_ERROR, error)
        }
    }
}

/// `threads.stop`: terminates the thread's in-flight run.
///
/// Idempotent on purpose — a thread with no run in flight has nothing to stop,
/// and answering `{ "ok": true }` says the thread is not running, which is the
/// state the caller asked for. The cancellation itself is
/// [`AppState::stop_thread`], which shares the terminal path with every other
/// run outcome.
async fn stop_thread_route(
    State(state): State<AppState>,
    Path(raw_thread_id): Path<String>,
) -> Response {
    let thread_id = match parse_thread_id(&raw_thread_id) {
        Ok(thread_id) => thread_id,
        Err(response) => return response,
    };
    if let Err(response) = public_thread_or_response(&state, &thread_id) {
        return response;
    }
    match state.stop_thread(&thread_id) {
        crate::runs::StopOutcome::Stopped | crate::runs::StopOutcome::NoRun => {
            Json(json!({ "ok": true })).into_response()
        }
        crate::runs::StopOutcome::PublishFailed { error } => {
            error_response(StatusCode::INTERNAL_SERVER_ERROR, error)
        }
    }
}

/// Folds every run event of a thread into the assistant messages it carried.
///
/// The ordering rule lives in [`crate::assistant_timeline`]; this is only the
/// adapter from the stored [`DomainEvent`] shape to it, so `threads.timeline`
/// and `threads.output` share one accumulator.
fn assistant_message_timeline(
    entries: &[(String, u64, u64, DomainEvent)],
) -> crate::assistant_timeline::AssistantMessageTimeline {
    let mut timeline = crate::assistant_timeline::AssistantMessageTimeline::new();
    for (_event_id, sequence, _created_at_ms, event) in entries {
        if let DomainEvent::ThreadRunEvent { run } = event {
            timeline.absorb(&run.run_id.to_string(), &run.event.body, *sequence);
        }
    }
    timeline
}

/// The thinking of a thread, folded from the deltas that streamed it.
fn reasoning_timeline(
    entries: &[(String, u64, u64, DomainEvent)],
) -> crate::reasoning_timeline::ReasoningTimeline {
    let mut timeline = crate::reasoning_timeline::ReasoningTimeline::new();
    for (_event_id, sequence, created_at_ms, event) in entries {
        if let DomainEvent::ThreadRunEvent { run } = event {
            timeline.absorb(
                &run.run_id.to_string(),
                &run.event.body,
                *sequence,
                *created_at_ms,
            );
        }
    }
    timeline
}

/// One timeline row for a folded thinking item.
fn reasoning_row(
    thread_id: &ThreadId,
    run_id: &str,
    item: &crate::reasoning_timeline::ReasoningItem,
) -> Value {
    json!({
        "id": format!("{run_id}-reasoning-{}", item.id.item_id),
        "threadId": thread_id.to_string(),
        "turnId": run_id,
        "sourceSeqStart": item.start_sequence,
        "sourceSeqEnd": item.end_sequence,
        "startedAt": item.started_at_ms,
        "createdAt": item.ended_at_ms,
        "completedAt": item.ended_at_ms,
        "kind": "system",
        "systemKind": "operation",
        "operationKind": "reasoning",
        // The client keys a thinking row's expanded state on this.
        "reasoningId": item.id.item_id,
        "title": format!("Thought for {}", item.duration_label()),
        "detail": item.text,
        "status": "completed",
    })
}

/// The tool calls of a thread, folded from the frames that described them.
fn tool_timeline(
    entries: &[(String, u64, u64, DomainEvent)],
) -> crate::tool_timeline::ToolTimeline {
    let mut timeline = crate::tool_timeline::ToolTimeline::new();
    for (_event_id, sequence, created_at_ms, event) in entries {
        if let DomainEvent::ThreadRunEvent { run } = event {
            timeline.absorb(
                &run.run_id.to_string(),
                &run.event.body,
                *sequence,
                *created_at_ms,
            );
        }
    }
    timeline
}

/// The item id a tool frame names, when it is one.
///
/// The three frames that make up a call are recognized here rather than inside
/// the fold because the row is emitted by the *first* of them: a row per frame
/// is what this projection exists to avoid.
fn tool_frame_item_id(event: &loom_domain::ProviderEvent) -> Option<&str> {
    match event {
        loom_domain::ProviderEvent::ItemStarted { item, .. }
            if crate::tool_timeline::ToolActivity::is_tool(item) =>
        {
            Some(tool_item_id(item))
        }
        loom_domain::ProviderEvent::ItemToolCallProgress { item_id, .. } => Some(item_id),
        loom_domain::ProviderEvent::ItemCompleted { item, .. }
            if crate::tool_timeline::ToolActivity::is_tool(item) =>
        {
            Some(tool_item_id(item))
        }
        _ => None,
    }
}

/// The id a tool item carries.
fn tool_item_id(item: &loom_domain::ThreadEventItem) -> &str {
    match item {
        loom_domain::ThreadEventItem::ToolCall { id, .. }
        | loom_domain::ThreadEventItem::CommandExecution { id, .. }
        | loom_domain::ThreadEventItem::FileChange { id, .. }
        | loom_domain::ThreadEventItem::FileRead { id, .. }
        | loom_domain::ThreadEventItem::Search { id, .. }
        | loom_domain::ThreadEventItem::WebFetch { id, .. } => id,
        _ => "",
    }
}

/// One timeline row for a folded assistant message.
///
/// The row id is the provider's item id prefixed with the thread, matching the
/// identity the client already keys a message on, and the row spans the first
/// and last frame that contributed to it.
fn assistant_timeline_row(
    run_id: &str,
    thread_id: &ThreadId,
    created_at_ms: u64,
    message: &crate::assistant_timeline::AssistantMessage,
) -> Value {
    let mut row = timeline_row_base(
        format!("{thread_id}:{run_id}:{}", message.id.item_id),
        thread_id,
        Some(run_id.to_owned()),
        message.start_sequence,
        created_at_ms,
    );
    let object = row.as_object_mut().expect("a timeline row is an object");
    object.insert(
        "sourceSeqEnd".into(),
        json!(message.end_sequence.max(message.start_sequence)),
    );
    for (key, value) in message.row_fields() {
        object.insert(key.into(), value);
    }
    row
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
        DomainEvent::ThreadRunEvent { run } => {
            let event_value =
                serde_json::to_value(&run.event).expect("ThreadEvent always serializes");
            match event_value.get("type").and_then(Value::as_str) {
                // An assistant delta is not a row: `threads.timeline` folds the
                // deltas that share an item id into the message they spell, and
                // emits that one row through
                // `assistant_message_timeline`/`assistant_timeline_row`. A row
                // per delta is what chopped a streamed answer into its chunks.
                Some("item/agentMessage/delta") => return None,
                Some("item/completed")
                    if event_value
                        .get("item")
                        .and_then(|item| item.get("type"))
                        .and_then(Value::as_str)
                        == Some("agentMessage") =>
                {
                    return None;
                }
                Some("provider/error") => {
                    let title = event_value
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("Provider error");
                    let detail = event_value
                        .get("detail")
                        .and_then(Value::as_str)
                        .map_or(Value::Null, |detail| json!(detail));
                    object.extend([
                        ("kind".into(), json!("system")),
                        ("title".into(), json!(title)),
                        ("detail".into(), detail),
                        ("status".into(), json!("error")),
                        ("systemKind".into(), json!("error")),
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
    if let Err(response) = public_thread_or_response(&state, &thread_id) {
        return response;
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
    // Assistant answers are folded per message *before* rows are built: the
    // contract carries an answer as many deltas sharing one item, while a
    // timeline row is one message. Folding at the row-building seam is what
    // keeps a streamed answer from becoming one row per chunk.
    //
    // The folded rows are emitted *after* the per-event rows and then sorted by
    // `sourceSeqStart`, so an assistant message sorts where it began streaming
    // while every other row keeps its own sequence. A row is emitted by the
    // first assistant frame that names a message the fold actually created —
    // which is the frame carrying its first text, not an earlier empty delta,
    // and which is exactly the set of messages the fold holds.
    let assistant_messages = assistant_message_timeline(&entries);
    // Thinking folds the same way and for the same reason: the contract carries
    // it as deltas that share an item, while a row is one "Thought for 1.2s"
    // line. Without this fold the reasoning never reaches a client at all — the
    // rows are built here, not in the browser.
    let reasoning_items = reasoning_timeline(&entries);
    // A tool call is several frames too — a start, progress, a completion — and
    // its row is the call, so it folds the same way before any row is built.
    let tool_items = tool_timeline(&entries);
    let mut emitted: HashSet<(String, String)> = HashSet::new();
    let mut emitted_reasoning: HashSet<(String, String)> = HashSet::new();
    let mut emitted_tools: HashSet<(String, String)> = HashSet::new();
    let mut assistant_rows: Vec<Value> = Vec::new();
    let mut reasoning_rows: Vec<Value> = Vec::new();
    let mut tool_rows: Vec<Value> = Vec::new();
    let mut model_fallback: Option<Value> = None;
    let mut all_rows = entries
        .iter()
        .filter_map(|(_event_id, sequence, created_at_ms, event)| {
            if let DomainEvent::ThreadRunEvent { run } = event {
                let run_id = run.run_id.to_string();
                if let loom_domain::ProviderEvent::ItemReasoningTextDelta { item_id, .. } =
                    &run.event.body
                {
                    // The frame belongs to the reasoning projection either way,
                    // so it never falls through to a generic row; it only
                    // contributes one when it is the first frame of an item the
                    // fold found something to say about.
                    if emitted_reasoning.insert((run_id.clone(), item_id.clone())) {
                        if let Some(item) = reasoning_items.get(&run_id, item_id) {
                            reasoning_rows.push(reasoning_row(&thread_id, &run_id, item));
                        }
                    }
                    return None;
                }
                if let Some(item_id) = tool_frame_item_id(&run.event.body) {
                    // A tool frame belongs to the tool projection either way, so
                    // it never falls through to a generic row; it contributes
                    // one only when it is the first frame of a call the fold
                    // holds.
                    if emitted_tools.insert((run_id.clone(), item_id.to_owned())) {
                        if let Some(activity) = tool_items.get(&run_id, item_id) {
                            tool_rows.extend(activity.rows(&thread_id.to_string()));
                        }
                    }
                    return None;
                }
                if let loom_domain::ProviderEvent::ProviderModelFallback {
                    original_model,
                    fallback_model: replaced_by,
                    ..
                } = &run.event.body
                {
                    model_fallback = Some(json!({
                        "originalModel": original_model,
                        "fallbackModel": replaced_by,
                        "detectedAt": created_at_ms,
                    }));
                }
                let item_id = match &run.event.body {
                    loom_domain::ProviderEvent::ItemAgentMessageDelta { item_id, .. } => {
                        Some(item_id.as_str())
                    }
                    loom_domain::ProviderEvent::ItemCompleted {
                        item: loom_domain::ThreadEventItem::AgentMessage { id, .. },
                        ..
                    } => Some(id.as_str()),
                    _ => None,
                };
                if let Some(item_id) = item_id {
                    // The frame belongs to the assistant projection either way,
                    // so it never falls through to a generic row; it only
                    // contributes one when it is the first frame of a message
                    // the fold created.
                    if emitted.insert((run_id.clone(), item_id.to_owned())) {
                        if let Some(message) = assistant_messages.get(&run_id, item_id) {
                            assistant_rows.push(assistant_timeline_row(
                                &run_id,
                                &thread_id,
                                *created_at_ms,
                                message,
                            ));
                        }
                    }
                    return None;
                }
            }
            timeline_row_for_event(&thread_id, *sequence, event)
        })
        .collect::<Vec<_>>();
    all_rows.extend(assistant_rows);
    all_rows.extend(reasoning_rows);
    all_rows.extend(tool_rows);
    all_rows.sort_by_key(|row| {
        row.get("sourceSeqStart")
            .and_then(Value::as_u64)
            .unwrap_or_default()
    });
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
            after.is_none_or(|value| sequence > value)
                && before.is_none_or(|value| sequence < value)
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
        "goal": goal_value(&state, &thread_id, &entries),
        "modelFallback": model_fallback,
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

// --- B3: interactions, plans and the queue --------------------------------

/// The `input` array bb's contract requires for a queued message, projected
/// from the stored text.
///
/// loom stores the prompt as text and rebuilds the contract's content array on
/// the way out. That round-trip is lossy in one direction only — a block loom
/// cannot deliver is refused at the route rather than stored and dropped — so a
/// client always sees back exactly what it may send.
fn queued_message_content(text: &str) -> Value {
    json!([{ "type": "text", "text": text, "mentions": [] }])
}

/// A stored prompt as the text a queued message's `input` carries.
///
/// Only the text variant is accepted (see [`text_from_queued_input`]), so the
/// projection is the concatenation of every text block. A block that is not
/// text was refused at creation and can never appear here.
fn text_from_queued_input(input: &Value) -> Result<String, String> {
    let Some(blocks) = input.as_array() else {
        return Err("input must be an array".into());
    };
    if blocks.is_empty() {
        return Err("input must contain at least one block".into());
    }
    let mut text = String::new();
    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                let Some(value) = block.get("text").and_then(Value::as_str) else {
                    return Err("a text block needs a `text` string".into());
                };
                text.push_str(value);
            }
            Some(other) => {
                // An image or a file cannot be delivered: loom's provider
                // protocol carries a prompt as text. Accepting it and dropping
                // it would be the silent data loss the batch's acceptance
                // criteria forbid, so it is refused with the block named.
                return Err(format!(
                    "loom's provider protocol delivers prompts as text; the `{other}` input block 
                     cannot be queued"
                ));
            }
            None => return Err("an input block needs a `type`".into()),
        }
    }
    if text.trim().is_empty() {
        return Err("input must contain text".into());
    }
    Ok(text)
}

/// Why a queued message is not deliverable yet, when the caller knows.
///
/// The contract's `waitingOn` union has seven variants; loom can construct
/// three of them honestly. `time` and `thread-busy` are the two states the
/// queue itself has, and `null` is "nothing known is holding it" — which is the
/// truthful answer for a message that simply has not been drained yet. The
/// remaining variants describe conditions loom does not model (a plugin
/// claim, host-offline queueing, provisioning) and are never fabricated.
fn queued_message_value(state: &AppState, message: &QueuedMessage) -> Value {
    let model = message
        .model
        .clone()
        .unwrap_or_else(|| configured_provider_id(state));
    json!({
        "id": message.id.to_string(),
        "initiator": message.initiator.as_str(),
        "senderThreadId": message.sender_thread_id.as_ref().map(ToString::to_string),
        "threadId": message.thread_id.to_string(),
        "content": queued_message_content(&message.text),
        "model": model,
        "reasoningLevel": message.reasoning_level.clone().map_or_else(
            || DEFAULT_REASONING_LEVEL.to_owned(),
            |level| level.to_string(),
        ),
        "permissionMode": message
            .permission_mode
            .clone()
            .unwrap_or_else(|| DEFAULT_PERMISSION_MODE.to_owned()),
        "serviceTier": message.service_tier.as_str(),
        "groupWithNext": message.group_with_next,
        "sendAt": message.send_at,
        "waitingOn": Value::Null,
        "failureReason": message.failure_reason,
        "payload": queued_message_payload_value(message),
        "editable": message.status.is_open(),
        "createdAt": message.created_at_ms,
        "updatedAt": message.updated_at_ms,
    })
}

/// A queued message's `payload`, in the contract's discriminated shape.
fn queued_message_payload_value(message: &QueuedMessage) -> Value {
    match &message.payload {
        QueuedMessagePayload::Inline => json!({ "kind": "inline" }),
        QueuedMessagePayload::Retry {
            retry_of_turn_request_id,
            attempt,
            reason,
        } => json!({
            "kind": "retry",
            "retryOfTurnRequestId": retry_of_turn_request_id,
            "attempt": attempt,
            "reason": reason,
        }),
    }
}

/// A queued-message row with its derived `waitingOn`.
fn queued_message_row(state: &AppState, message: &QueuedMessage, now: u64) -> Value {
    let mut value = queued_message_value(state, message);
    let waiting_on = match message.status {
        QueuedMessageStatus::Queued if !message.is_due(now) => json!({ "kind": "time" }),
        // A queued message whose thread is busy is exactly `thread-busy`; a
        // queued message whose thread is idle simply has not been drained yet,
        // and `null` says that rather than inventing a cause.
        QueuedMessageStatus::Queued => state
            .registry
            .thread(&message.thread_id)
            .filter(|thread| !thread.status.accepts_work())
            .map_or(Value::Null, |_| json!({ "kind": "thread-busy" })),
        _ => Value::Null,
    };
    if let Value::Object(object) = &mut value {
        object.insert("waitingOn".into(), waiting_on);
    }
    value
}

/// Query of a `queue.list` read.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
struct QueueListQuery {
    thread_id: Option<String>,
    /// Accepted for wire compatibility; loom has no plugin queue claim, so it
    /// does not filter on it.
    wait_holder: Option<String>,
}

/// `queue.list`: every queued message still waiting, oldest first.
///
/// A bare array, not an envelope. A narrowed read (`threadId`) is the same
/// array filtered; `waitHolder` is accepted and ignored, because loom has no
/// plugin claim on a queue and reporting a filtered list would be worse than
/// reporting the whole one.
async fn list_queued_messages(
    State(state): State<AppState>,
    Query(query): Query<QueueListQuery>,
) -> Response {
    let thread_id = match query.thread_id.as_deref() {
        None | Some("") => None,
        Some(raw) => match raw.parse::<ThreadId>() {
            Ok(thread_id) => Some(thread_id),
            Err(error) => return error_response(StatusCode::BAD_REQUEST, error.to_string()),
        },
    };
    if let Some(thread_id) = &thread_id {
        if let Err(response) = public_thread_or_response(&state, thread_id) {
            return response;
        }
    }
    let now = loom_relay::now_ms();
    Json(
        state
            .registry
            .queued_messages_for(thread_id.as_ref())
            .into_iter()
            .filter(|message| message.status == QueuedMessageStatus::Queued)
            .map(|message| queued_message_row(&state, &message, now))
            .collect::<Vec<_>>(),
    )
    .into_response()
}

/// `threads.queuedMessages`: the thread's queue, oldest first.
async fn thread_queued_messages(
    State(state): State<AppState>,
    Path(raw_thread_id): Path<String>,
) -> Response {
    let thread_id = match parse_thread_id(&raw_thread_id) {
        Ok(thread_id) => thread_id,
        Err(response) => return response,
    };
    if let Err(response) = public_thread_or_response(&state, &thread_id) {
        return response;
    }
    let now = loom_relay::now_ms();
    Json(
        state
            .registry
            .queued_messages_for(Some(&thread_id))
            .into_iter()
            .filter(|message| message.status == QueuedMessageStatus::Queued)
            .map(|message| queued_message_row(&state, &message, now))
            .collect::<Vec<_>>(),
    )
    .into_response()
}

/// Body of a `threads.createQueuedMessage` request.
///
/// Every field but `input` is optional in the contract; `input` is lifted to a
/// [`Value`] so the handler can refuse a non-text block with a message that
/// names it, rather than a serde error the client cannot act on.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateQueuedMessageRequest {
    input: Value,
    #[serde(default)]
    sender_thread_id: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    reasoning_level: Option<ReasoningLevel>,
    #[serde(default)]
    permission_mode: Option<String>,
    #[serde(default)]
    service_tier: Option<String>,
}

/// `threads.createQueuedMessage`: stores a prompt for a busy thread.
///
/// The message is durable state, not a held request: the client that created it
/// may disconnect and the server may restart, and `threads.queuedMessages` is
/// still expected to answer it. It is delivered by a run reaching a terminal
/// state (`AppState::drain_thread_queue`) or by `threads.sendQueuedMessage`.
async fn create_queued_message(
    State(state): State<AppState>,
    Path(raw_thread_id): Path<String>,
    Json(request): Json<CreateQueuedMessageRequest>,
) -> Response {
    let thread_id = match parse_thread_id(&raw_thread_id) {
        Ok(thread_id) => thread_id,
        Err(response) => return response,
    };
    if let Err(response) = public_thread_or_response(&state, &thread_id) {
        return response;
    }
    let text = match text_from_queued_input(&request.input) {
        Ok(text) => text,
        Err(message) => {
            return error_response_with_code(StatusCode::BAD_REQUEST, "invalid_request", message)
        }
    };
    let sender_thread_id = match request.sender_thread_id.as_deref() {
        None | Some("") => None,
        Some(raw) => match raw.parse::<ThreadId>() {
            Ok(thread_id) => Some(thread_id),
            Err(error) => return error_response(StatusCode::BAD_REQUEST, error.to_string()),
        },
    };
    let service_tier = match request.service_tier.as_deref() {
        None | Some("default") => ServiceTier::Default,
        Some("fast") => ServiceTier::Fast,
        Some(other) => {
            return error_response_with_code(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                format!("serviceTier must be `fast` or `default`, got {other:?}"),
            )
        }
    };
    let now = loom_relay::now_ms();
    match state.registry.create_queued_message(
        NewQueuedMessage {
            thread_id: thread_id.clone(),
            sender_thread_id,
            initiator: QueuedMessageInitiator::User,
            text,
            model: request.model,
            reasoning_level: request.reasoning_level,
            permission_mode: request.permission_mode,
            service_tier,
            group_with_next: false,
            send_at: None,
            payload: QueuedMessagePayload::Inline,
        },
        now,
    ) {
        Ok((message, event)) => {
            let _ = state.publish_domain_event(&event);
            (
                StatusCode::CREATED,
                Json(queued_message_row(&state, &message, now)),
            )
                .into_response()
        }
        Err(error) => command_error_response(error),
    }
}

/// Body of a `threads.sendQueuedMessage` request.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SendQueuedMessageRequest {
    mode: SendQueuedMessageMode,
}

/// bb's `mode` for sending a queued message.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum SendQueuedMessageMode {
    /// Send as a normal turn, or leave it queued if the thread is busy.
    Auto,
    /// Inject into the running turn.
    Steer,
}

/// `threads.sendQueuedMessage`: delivers a queued message, or says why not.
///
/// The two contract branches are the two honest outcomes:
///
/// * `delivery: "sent"` — the message became a turn, now;
/// * `delivery: "queued"` — it is still in the queue, with a `waitingOn` that
///   names the reason and a `queuedMessage` the client re-renders.
///
/// `steer` is refused with `501 not_configured` rather than downgraded to a
/// queued send. `loom_provider_protocol` has no frame that injects input into a
/// running turn, so the only way to honour `steer` would be to append a second
/// concurrent turn — which is a different operation wearing the same name.
async fn send_queued_message(
    State(state): State<AppState>,
    Path((raw_thread_id, raw_queued_message_id)): Path<(String, String)>,
    Json(request): Json<SendQueuedMessageRequest>,
) -> Response {
    let thread_id = match parse_thread_id(&raw_thread_id) {
        Ok(thread_id) => thread_id,
        Err(response) => return response,
    };
    if let Err(response) = public_thread_or_response(&state, &thread_id) {
        return response;
    }
    let queued_message_id = match raw_queued_message_id.parse::<QueuedMessageId>() {
        Ok(id) => id,
        Err(error) => return error_response(StatusCode::BAD_REQUEST, error.to_string()),
    };
    let Some(message) = state.registry.queued_message(&queued_message_id) else {
        return error_response(
            StatusCode::NOT_FOUND,
            format!("queued message {queued_message_id} is not known"),
        );
    };
    if message.thread_id != thread_id {
        return error_response(
            StatusCode::NOT_FOUND,
            format!("queued message {queued_message_id} belongs to another thread"),
        );
    }
    if request.mode == SendQueuedMessageMode::Steer {
        return error_response_with_code(
            StatusCode::NOT_IMPLEMENTED,
            "not_configured",
            format!(
                "loom's provider protocol has no frame that injects input into a running turn; \
                 queued message {queued_message_id} cannot be steered"
            ),
        );
    }

    let now = loom_relay::now_ms();
    // `force` is what makes a manual send authoritative over `sendAt`: the
    // client is asking for this message now, which is exactly the schedule it
    // set. The automatic drain does the opposite, because there the schedule is
    // the whole point.
    match state.deliver_queued_message(&queued_message_id, true, now) {
        DeliveryOutcome::Sent(message) => Json(json!({
            "ok": true,
            "delivery": "sent",
            "queuedMessage": queued_message_row(&state, &message, now),
        }))
        .into_response(),
        DeliveryOutcome::StillQueued {
            message,
            waiting_on,
        } => {
            let row = queued_message_row(&state, &message, now);
            let mut row = row;
            if let Value::Object(object) = &mut row {
                object.insert("waitingOn".into(), waiting_on.value());
            }
            Json(json!({
                "ok": true,
                "delivery": "queued",
                "queuedMessage": row,
            }))
            .into_response()
        }
        DeliveryOutcome::Failed { message, .. } => {
            // A failure still leaves the message queued; the contract's queued
            // branch is the honest answer, with the reason in the row.
            let row = queued_message_row(&state, &message, now);
            Json(json!({
                "ok": true,
                "delivery": "queued",
                "queuedMessage": row,
            }))
            .into_response()
        }
        DeliveryOutcome::Unknown => error_response_with_code(
            StatusCode::CONFLICT,
            "queued_message_claim_lost",
            format!("queued message {queued_message_id} is no longer queued"),
        ),
    }
}

#[allow(clippy::result_large_err)]
fn parse_queued_message_id(raw: &str) -> Result<QueuedMessageId, Response> {
    raw.parse::<QueuedMessageId>()
        .map_err(|error| error_response(StatusCode::BAD_REQUEST, error.to_string()))
}

#[allow(clippy::result_large_err)]
fn parse_optional_queued_message_id(
    raw: Option<&str>,
) -> Result<Option<QueuedMessageId>, Response> {
    raw.filter(|value| !value.is_empty())
        .map(parse_queued_message_id)
        .transpose()
}

#[allow(clippy::result_large_err)]
fn queued_message_for_thread(
    state: &AppState,
    thread_id: &ThreadId,
    queued_message_id: &QueuedMessageId,
) -> Result<QueuedMessage, Response> {
    let Some(message) = state.registry.queued_message(queued_message_id) else {
        return Err(error_response(
            StatusCode::NOT_FOUND,
            format!("queued message {queued_message_id} is not known"),
        ));
    };
    if message.thread_id != *thread_id {
        return Err(error_response(
            StatusCode::NOT_FOUND,
            format!("queued message {queued_message_id} belongs to another thread"),
        ));
    }
    Ok(message)
}

/// Deletes a queued row by moving it to the durable `cancelled` terminal
/// state. The row remains in snapshots and replay even though queue reads omit
/// terminal messages.
async fn delete_queued_message(
    State(state): State<AppState>,
    Path((raw_thread_id, raw_queued_message_id)): Path<(String, String)>,
) -> Response {
    let thread_id = match parse_thread_id(&raw_thread_id) {
        Ok(thread_id) => thread_id,
        Err(response) => return response,
    };
    if let Err(response) = public_thread_or_response(&state, &thread_id) {
        return response;
    }
    let queued_message_id = match parse_queued_message_id(&raw_queued_message_id) {
        Ok(id) => id,
        Err(response) => return response,
    };
    let message = match queued_message_for_thread(&state, &thread_id, &queued_message_id) {
        Ok(message) => message,
        Err(response) => return response,
    };
    if !message.status.is_open() {
        return error_response_with_code(
            StatusCode::CONFLICT,
            "queued_message_claim_lost",
            format!(
                "queued message {queued_message_id} is already {}",
                message.status
            ),
        );
    }
    match state
        .registry
        .cancel_queued_message(&queued_message_id, loom_relay::now_ms())
    {
        Ok((_message, event)) => match state.publish_domain_event(&event) {
            Ok(_) => lifecycle_ok(),
            Err(error) => error_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
        },
        Err(error) => command_error_response(error),
    }
}

/// Body of `threads.updateQueuedMessage`.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpdateQueuedMessageRequest {
    expected_updated_at: u64,
    input: Value,
}

/// Updates a still-queued prompt under the timestamp CAS supplied by the UI.
async fn update_queued_message(
    State(state): State<AppState>,
    Path((raw_thread_id, raw_queued_message_id)): Path<(String, String)>,
    Json(request): Json<UpdateQueuedMessageRequest>,
) -> Response {
    let thread_id = match parse_thread_id(&raw_thread_id) {
        Ok(thread_id) => thread_id,
        Err(response) => return response,
    };
    if let Err(response) = public_thread_or_response(&state, &thread_id) {
        return response;
    }
    let queued_message_id = match parse_queued_message_id(&raw_queued_message_id) {
        Ok(id) => id,
        Err(response) => return response,
    };
    let message = match queued_message_for_thread(&state, &thread_id, &queued_message_id) {
        Ok(message) => message,
        Err(response) => return response,
    };
    if !message.status.is_open() {
        return error_response_with_code(
            StatusCode::CONFLICT,
            "queued_message_claim_lost",
            format!(
                "queued message {queued_message_id} is already {}",
                message.status
            ),
        );
    }
    let text = match text_from_queued_input(&request.input) {
        Ok(text) => text,
        Err(message) => {
            return error_response_with_code(StatusCode::BAD_REQUEST, "invalid_request", message)
        }
    };
    match state.registry.update_queued_message(
        &thread_id,
        &queued_message_id,
        request.expected_updated_at,
        text,
        loom_relay::now_ms(),
    ) {
        Ok((message, event)) => match state.publish_domain_event(&event) {
            Ok(_) => {
                Json(queued_message_row(&state, &message, loom_relay::now_ms())).into_response()
            }
            Err(error) => error_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
        },
        Err(CommandError::Conflict(message)) => {
            error_response_with_code(StatusCode::CONFLICT, "conflict", message)
        }
        Err(error) => command_error_response(error),
    }
}

/// Body of `threads.reorderQueuedMessage`.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReorderQueuedMessageRequest {
    #[serde(default)]
    group_boundary_queued_message_id: Option<String>,
    previous_queued_message_id: Option<String>,
    next_queued_message_id: Option<String>,
}

/// Reorders one queued row while retaining its grouping edges.
async fn reorder_queued_message(
    State(state): State<AppState>,
    Path((raw_thread_id, raw_queued_message_id)): Path<(String, String)>,
    Json(request): Json<ReorderQueuedMessageRequest>,
) -> Response {
    let thread_id = match parse_thread_id(&raw_thread_id) {
        Ok(thread_id) => thread_id,
        Err(response) => return response,
    };
    if let Err(response) = public_thread_or_response(&state, &thread_id) {
        return response;
    }
    let queued_message_id = match parse_queued_message_id(&raw_queued_message_id) {
        Ok(id) => id,
        Err(response) => return response,
    };
    let previous_id =
        match parse_optional_queued_message_id(request.previous_queued_message_id.as_deref()) {
            Ok(id) => id,
            Err(response) => return response,
        };
    let next_id = match parse_optional_queued_message_id(request.next_queued_message_id.as_deref())
    {
        Ok(id) => id,
        Err(response) => return response,
    };
    let group_boundary_id =
        match parse_optional_queued_message_id(request.group_boundary_queued_message_id.as_deref())
        {
            Ok(id) => id,
            Err(response) => return response,
        };
    match state.registry.reorder_queued_message(
        &thread_id,
        &queued_message_id,
        previous_id.as_ref(),
        next_id.as_ref(),
        group_boundary_id.as_ref(),
        loom_relay::now_ms(),
    ) {
        Ok((messages, events)) => {
            if let Err(error) = publish_all(&state, &events) {
                return error_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string());
            }
            let now = loom_relay::now_ms();
            Json(
                messages
                    .iter()
                    .map(|message| queued_message_row(&state, message, now))
                    .collect::<Vec<_>>(),
            )
            .into_response()
        }
        Err(error) => command_error_response(error),
    }
}

/// Body of `threads.setQueuedMessageGroupBoundary`.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetQueuedMessageGroupBoundaryRequest {
    expected_grouped_prefix_queued_message_ids: Vec<String>,
    group_boundary_queued_message_id: String,
}

/// Changes the grouped prefix under an optimistic prefix check.
async fn set_queued_message_group_boundary(
    State(state): State<AppState>,
    Path(raw_thread_id): Path<String>,
    Json(request): Json<SetQueuedMessageGroupBoundaryRequest>,
) -> Response {
    let thread_id = match parse_thread_id(&raw_thread_id) {
        Ok(thread_id) => thread_id,
        Err(response) => return response,
    };
    if let Err(response) = public_thread_or_response(&state, &thread_id) {
        return response;
    }
    let mut expected_ids =
        Vec::with_capacity(request.expected_grouped_prefix_queued_message_ids.len());
    for raw_id in request.expected_grouped_prefix_queued_message_ids {
        match parse_queued_message_id(&raw_id) {
            Ok(id) => expected_ids.push(id),
            Err(response) => return response,
        }
    }
    let boundary_id = match parse_queued_message_id(&request.group_boundary_queued_message_id) {
        Ok(id) => id,
        Err(response) => return response,
    };
    match state.registry.set_queued_message_group_boundary(
        &thread_id,
        &expected_ids,
        &boundary_id,
        loom_relay::now_ms(),
    ) {
        Ok((messages, events)) => {
            if let Err(error) = publish_all(&state, &events) {
                return error_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string());
            }
            let now = loom_relay::now_ms();
            Json(
                messages
                    .iter()
                    .map(|message| queued_message_row(&state, message, now))
                    .collect::<Vec<_>>(),
            )
            .into_response()
        }
        Err(CommandError::Conflict(message)) => {
            error_response_with_code(StatusCode::CONFLICT, "conflict", message)
        }
        Err(error) => command_error_response(error),
    }
}

/// An interaction as the contract's interaction union.
///
/// The four variants share one row shape and differ in `payload` and
/// `resolution`; seeing one shape rather than four projections is the point of
/// the domain's [`InteractionPayload`] and [`Resolution`].
fn interaction_value(state: &AppState, interaction: &Interaction) -> Value {
    // The agent's own conversation id when the request came from a provider
    // that had named one; a request with no session (or a plugin's) falls back
    // to loom's thread id, which is the only identity it has.
    let provider_thread_id = interaction
        .provider_thread_id
        .clone()
        .unwrap_or_else(|| interaction.thread_id.to_string());
    let origin = match &interaction.origin {
        InteractionOrigin::Provider {
            provider_id,
            provider_request_id,
        } => json!({
            "kind": "provider",
            "providerId": provider_id,
            "providerThreadId": provider_thread_id,
            "providerRequestId": provider_request_id,
        }),
        InteractionOrigin::Plugin {
            plugin_id,
            renderer_id,
        } => json!({
            "kind": "plugin",
            "pluginId": plugin_id,
            "rendererId": renderer_id,
        }),
    };
    let mut value = json!({
        "id": interaction.id.to_string(),
        "threadId": interaction.thread_id.to_string(),
        "status": interaction.status.as_str(),
        "statusReason": interaction.status_reason,
        "createdAt": interaction.created_at_ms,
        "resolvedAt": interaction.resolved_at_ms,
        "turnId": interaction.turn_id,
        "providerId": configured_provider_id(state),
        "providerThreadId": provider_thread_id,
        "providerRequestId": match &interaction.origin {
            InteractionOrigin::Provider { provider_request_id, .. } => provider_request_id.clone(),
            InteractionOrigin::Plugin { plugin_id, .. } => plugin_id.clone(),
        },
        "payload": interaction.payload.value(),
        "resolution": interaction.resolution.as_ref().map(Resolution::value),
        "expiresAt": interaction.expires_at_ms,
        "origin": origin,
    });
    // The plugin variant of the union has no provider triple at all, and
    // `additionalProperties: false` means leaving the fields in rejects the
    // row. `origin` is what tells a client which union branch it is looking at.
    if matches!(interaction.origin, InteractionOrigin::Plugin { .. }) {
        if let Value::Object(object) = &mut value {
            object.remove("providerId");
            object.remove("providerThreadId");
            object.remove("providerRequestId");
        }
    }
    value
}

/// `threads.interactions`: every interaction a thread has raised, oldest first.
///
/// The whole history, not only the pending ones: a client renders answered
/// questions inline in the timeline, and a settled interaction is what makes a
/// tool-approval row stay answered after a reload.
async fn thread_interactions(
    State(state): State<AppState>,
    Path(raw_thread_id): Path<String>,
) -> Response {
    let thread_id = match parse_thread_id(&raw_thread_id) {
        Ok(thread_id) => thread_id,
        Err(response) => return response,
    };
    if let Err(response) = public_thread_or_response(&state, &thread_id) {
        return response;
    }
    Json(
        state
            .registry
            .interactions_for(Some(&thread_id))
            .iter()
            .map(|interaction| interaction_value(&state, interaction))
            .collect::<Vec<_>>(),
    )
    .into_response()
}

/// Resolves an interaction by id, refusing one that belongs to another thread.
#[allow(clippy::result_large_err)]
fn interaction_in_thread(
    state: &AppState,
    thread_id: &ThreadId,
    raw_interaction_id: &str,
) -> Result<Interaction, Response> {
    let interaction_id = raw_interaction_id
        .parse::<InteractionId>()
        .map_err(|error| error_response(StatusCode::BAD_REQUEST, error.to_string()))?;
    let Some(interaction) = state.registry.interaction(&interaction_id) else {
        return Err(error_response(
            StatusCode::NOT_FOUND,
            format!("interaction {interaction_id} is not known"),
        ));
    };
    if &interaction.thread_id != thread_id {
        return Err(error_response(
            StatusCode::NOT_FOUND,
            format!("interaction {interaction_id} belongs to another thread"),
        ));
    }
    Ok(interaction)
}

/// `threads.interaction`: one interaction by id.
async fn thread_interaction(
    State(state): State<AppState>,
    Path((raw_thread_id, raw_interaction_id)): Path<(String, String)>,
) -> Response {
    let thread_id = match parse_thread_id(&raw_thread_id) {
        Ok(thread_id) => thread_id,
        Err(response) => return response,
    };
    if let Err(response) = public_thread_or_response(&state, &thread_id) {
        return response;
    }
    match interaction_in_thread(&state, &thread_id, &raw_interaction_id) {
        Ok(interaction) => Json(interaction_value(&state, &interaction)).into_response(),
        Err(response) => response,
    }
}

/// Body of a `threads.respondToInteraction` request.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RespondToInteractionRequest {
    value: Value,
}

/// `threads.respondToInteraction`: answers with an opaque value.
///
/// This is the generic verb: the client supplies a `value` and loom records it
/// verbatim as a `request_answer`. It is deliberately *not* the same operation
/// as [`resolve_thread_interaction`] — that one carries a typed resolution and
/// validates it against the request's kind, so a client cannot answer an
/// approval with a question's answers. Using `respond` for a typed interaction
/// is refused here rather than accepted and stored under the wrong shape.
async fn respond_to_thread_interaction(
    State(state): State<AppState>,
    Path((raw_thread_id, raw_interaction_id)): Path<(String, String)>,
    Json(request): Json<RespondToInteractionRequest>,
) -> Response {
    let thread_id = match parse_thread_id(&raw_thread_id) {
        Ok(thread_id) => thread_id,
        Err(response) => return response,
    };
    if let Err(response) = public_thread_or_response(&state, &thread_id) {
        return response;
    }
    let interaction = match interaction_in_thread(&state, &thread_id, &raw_interaction_id) {
        Ok(interaction) => interaction,
        Err(response) => return response,
    };
    let resolution = Resolution::RequestAnswer {
        value: request.value,
    };
    if !resolution.answers(interaction.kind) {
        return error_response_with_code(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            format!(
                "interaction {} is a {} and takes a typed resolution, not an opaque value",
                interaction.id, interaction.kind
            ),
        );
    }
    deliver_interaction_response(&state, &interaction, resolution)
}

/// Body of a `threads.resolveInteraction` request.
///
/// The contract's union has four branches and they are not interchangeable, so
/// the body is parsed into the domain's [`Resolution`] directly: a serde
/// failure for an unknown branch is the same `422` the contract middleware
/// would produce, and a *known* branch that does not match the interaction's
/// kind is a `400` from the handler below.
#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
enum ResolveInteractionRequest {
    /// `{ decision, grantedPermissions }` — a permission decision.
    Decision(DecisionRequest),
    /// `{ kind: "user_answer", answers }`.
    UserAnswer(UserAnswerRequest),
    /// `{ kind: "plugin_submitted" }`.
    PluginSubmitted(PluginSubmittedRequest),
    /// `{ kind: "request_answer", value }`.
    RequestAnswer(RequestAnswerRequest),
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DecisionRequest {
    decision: String,
    #[serde(default, deserialize_with = "deserialize_present_json")]
    granted_permissions: Option<Value>,
}

/// Keeps a required nullable JSON field distinct from an omitted field.
///
/// Serde's ordinary `Option<Value>` maps both JSON `null` and a missing key to
/// `None`. The public contract requires `grantedPermissions` on allow decisions
/// while explicitly permitting `null`, so a present value is wrapped in
/// `Some` even when that value is [`Value::Null`]. `#[serde(default)]` above
/// remains the missing-key path.
fn deserialize_present_json<'de, D>(deserializer: D) -> Result<Option<Value>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Value::deserialize(deserializer).map(Some)
}

#[derive(Clone, Debug, Deserialize)]
struct UserAnswerRequest {
    kind: String,
    answers: Value,
}

#[derive(Clone, Debug, Deserialize)]
struct PluginSubmittedRequest {
    kind: String,
}

#[derive(Clone, Debug, Deserialize)]
struct RequestAnswerRequest {
    kind: String,
    value: Value,
}

impl ResolveInteractionRequest {
    /// The domain resolution this body names.
    fn resolution(self) -> Resolution {
        match self {
            ResolveInteractionRequest::Decision(request) => Resolution::Decision {
                decision: request.decision,
                granted_permissions: request.granted_permissions,
            },
            ResolveInteractionRequest::UserAnswer(request) => Resolution::UserAnswer {
                answers: request.answers,
            },
            ResolveInteractionRequest::PluginSubmitted(_) => Resolution::PluginSubmitted,
            ResolveInteractionRequest::RequestAnswer(request) => Resolution::RequestAnswer {
                value: request.value,
            },
        }
    }

    /// A decision needs `grantedPermissions` unless it is a denial.
    ///
    /// The contract requires the field on the two `allow` branches and forbids
    /// it on `deny`; the request validator enforces the required half, and this
    /// is the forbidden half, which no schema can express as a plain
    /// `additionalProperties` rule.
    fn is_coherent(&self) -> Result<(), String> {
        match self {
            ResolveInteractionRequest::Decision(request) => match request.decision.as_str() {
                "allow_once" | "allow_for_session" => {
                    if request.granted_permissions.is_none() {
                        return Err(format!(
                            "decision `{}` needs `grantedPermissions`",
                            request.decision
                        ));
                    }
                    Ok(())
                }
                "deny" => {
                    if request.granted_permissions.is_some() {
                        return Err(
                            "a `deny` decision must not carry `grantedPermissions`".to_owned()
                        );
                    }
                    Ok(())
                }
                other => Err(format!(
                    "decision must be `allow_once`, `allow_for_session` or `deny`, got {other:?}"
                )),
            },
            ResolveInteractionRequest::UserAnswer(request) => {
                if request.kind != "user_answer" {
                    return Err(format!("unknown resolution kind {:?}", request.kind));
                }
                Ok(())
            }
            ResolveInteractionRequest::PluginSubmitted(request) => {
                if request.kind != "plugin_submitted" {
                    return Err(format!("unknown resolution kind {:?}", request.kind));
                }
                Ok(())
            }
            ResolveInteractionRequest::RequestAnswer(request) => {
                if request.kind != "request_answer" {
                    return Err(format!("unknown resolution kind {:?}", request.kind));
                }
                Ok(())
            }
        }
    }
}

/// `threads.resolveInteraction`: answers with a typed resolution.
///
/// This is the typed verb. The resolution is validated against the request's
/// own kind (`Resolution::answers`) so a decision cannot answer a question, and
/// it is refused rather than stored under a shape nothing will read. The three
/// interaction verbs are therefore distinct:
///
/// * `respond` — an opaque value for an interaction loom does not interpret;
/// * `resolve` — a typed answer, matched to the request's kind;
/// * `cancel` — no permission decision, settled as `interrupted` and reported
///   to a blocked provider as cancellation.
async fn resolve_thread_interaction(
    State(state): State<AppState>,
    Path((raw_thread_id, raw_interaction_id)): Path<(String, String)>,
    Json(request): Json<ResolveInteractionRequest>,
) -> Response {
    let thread_id = match parse_thread_id(&raw_thread_id) {
        Ok(thread_id) => thread_id,
        Err(response) => return response,
    };
    if let Err(response) = public_thread_or_response(&state, &thread_id) {
        return response;
    }
    let interaction = match interaction_in_thread(&state, &thread_id, &raw_interaction_id) {
        Ok(interaction) => interaction,
        Err(response) => return response,
    };
    if let Err(message) = request.is_coherent() {
        return error_response_with_code(StatusCode::BAD_REQUEST, "invalid_request", message);
    }
    let resolution = request.resolution();
    if !resolution.answers(interaction.kind) {
        return error_response_with_code(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            format!(
                "interaction {} is a {}; a {} cannot answer it",
                interaction.id,
                interaction.kind,
                resolution.kind()
            ),
        );
    }
    deliver_interaction_response(&state, &interaction, resolution)
}

/// Delivers a validated resolution, mapping the outcome onto the contract.
fn deliver_interaction_response(
    state: &AppState,
    interaction: &Interaction,
    resolution: Resolution,
) -> Response {
    let now = loom_relay::now_ms();
    match state.deliver_interaction_resolution(&interaction.id, resolution, now) {
        DeliverOutcome::Delivered(interaction) => {
            Json(interaction_value(state, &interaction)).into_response()
        }
        DeliverOutcome::DeliveryFailed { interaction, error } => error_response_with_code(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            format!(
                "interaction {} answer is queued for retry: {error}",
                interaction.id
            ),
        ),
        DeliverOutcome::Settled(interaction) => error_response_with_code(
            StatusCode::CONFLICT,
            "awaiting_user_interaction",
            format!(
                "interaction {} was already settled as {}",
                interaction.id, interaction.status
            ),
        ),
        DeliverOutcome::Unknown => error_response(
            StatusCode::NOT_FOUND,
            format!("interaction {} is not known", interaction.id),
        ),
    }
}

/// `threads.cancelInteraction`: settles an interaction without a decision.
///
/// Cancelling is the verb for "this will never be answered": the run was
/// stopped, the provider went away, or a client withdrew the prompt. It is
/// **not** a denial — a denial selects the provider's rejecting option, while a
/// cancellation returns ACP's `Cancelled` outcome. A settled interaction cannot
/// be cancelled twice: that is a race with whoever answered it, and reporting
/// success for it would hide the answer.
async fn cancel_thread_interaction(
    State(state): State<AppState>,
    Path((raw_thread_id, raw_interaction_id)): Path<(String, String)>,
) -> Response {
    let thread_id = match parse_thread_id(&raw_thread_id) {
        Ok(thread_id) => thread_id,
        Err(response) => return response,
    };
    if let Err(response) = public_thread_or_response(&state, &thread_id) {
        return response;
    }
    let interaction = match interaction_in_thread(&state, &thread_id, &raw_interaction_id) {
        Ok(interaction) => interaction,
        Err(response) => return response,
    };
    match state.cancel_interaction(
        &interaction.id,
        Some("cancelled by a client".into()),
        loom_relay::now_ms(),
    ) {
        DeliverOutcome::Delivered(interaction) => {
            Json(interaction_value(&state, &interaction)).into_response()
        }
        DeliverOutcome::DeliveryFailed { interaction, error } => error_response_with_code(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            format!(
                "interaction {} cancellation is queued for retry: {error}",
                interaction.id
            ),
        ),
        DeliverOutcome::Settled(interaction) => error_response_with_code(
            StatusCode::CONFLICT,
            "awaiting_user_interaction",
            format!(
                "interaction {} is already {}",
                interaction.id, interaction.status
            ),
        ),
        DeliverOutcome::Unknown => error_response(
            StatusCode::NOT_FOUND,
            format!("interaction {} is not known", interaction.id),
        ),
    }
}

/// Query of a `threads.eventWait` read.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct EventWaitQuery {
    /// The contract declares `type` required; loom records it and does not
    /// filter on it, because the polling loop is driven by sequence, not type.
    #[serde(rename = "type")]
    #[allow(dead_code)]
    event_type: Option<String>,
    after_seq: Option<String>,
    wait_ms: Option<String>,
}

/// The longest a `threads.eventWait` may hold a connection open.
///
/// A long poll without a ceiling is a connection leak with a friendly name; the
/// contract's `waitMs` is a request, and this is the bound the server applies
/// to it. The default is deliberately shorter than the worker heartbeat window
/// so a client that reconnects on timeout is never mistaken for a stale one.
const EVENT_WAIT_DEFAULT_MS: u64 = 30_000;
/// The hard ceiling on `waitMs`, so one request cannot pin a connection.
const EVENT_WAIT_MAX_MS: u64 = 60_000;
/// How often the wait re-reads the log while holding the connection.
///
/// The relay has no `wait for a new event` primitive: the log is a store and
/// the hub is a fan-out to sockets, and a producer deliberately cannot see who
/// is subscribed. So a long poll is a bounded poll of the log. 25 ms keeps the
/// latency below what a human notices while costing one shard read per waiting
/// client per tick — acceptable because a waiting client is one that has
/// nothing else to do.
const EVENT_WAIT_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(25);

/// `threads.eventWait`: holds a connection until the thread has a new event.
///
/// The cursor is the same one `threads.events` uses: the row's `seq`, derived
/// from the thread room's replay order, and `afterSeq` is exclusive. That is
/// what keeps a waiter from re-delivering an event it already applied.
///
/// The response is a **bare `ThreadEventRow`** on a match and JSON `null` on
/// timeout — the contract's declared union — so a client distinguishes "nothing
/// happened yet" from "here is what happened" without an error path. `200`
/// carries both, deliberately: a timeout is not a failure.
async fn thread_event_wait(
    State(state): State<AppState>,
    Path(raw_thread_id): Path<String>,
    Query(query): Query<EventWaitQuery>,
) -> Response {
    let thread_id = match parse_thread_id(&raw_thread_id) {
        Ok(thread_id) => thread_id,
        Err(response) => return response,
    };
    if let Err(response) = public_thread_or_response(&state, &thread_id) {
        return response;
    }
    let after = match parse_query_sequence(query.after_seq.as_ref(), "afterSeq") {
        Ok(value) => value,
        Err(response) => return response,
    };
    let wait_ms = match parse_query_sequence(query.wait_ms.as_ref(), "waitMs") {
        Ok(Some(value)) => value.min(EVENT_WAIT_MAX_MS),
        Ok(None) => EVENT_WAIT_DEFAULT_MS,
        Err(response) => return response,
    };

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(wait_ms);
    loop {
        let rows = match thread_event_rows(&state, &thread_id) {
            Ok(rows) => rows,
            Err(response) => return response,
        };
        if let Some(row) = rows
            .into_iter()
            .find(|(sequence, _)| match after {
                None => true,
                Some(cursor) => *sequence > cursor,
            })
            .map(|(_, row)| row)
        {
            return Json(row).into_response();
        }
        if tokio::time::Instant::now() >= deadline {
            return Json(Value::Null).into_response();
        }
        tokio::time::sleep(EVENT_WAIT_POLL_INTERVAL).await;
    }
}

/// Every contract event in a thread's room, in sequence order, with the
/// sequence a client uses as a cursor.
#[allow(clippy::result_large_err)]
fn thread_event_rows(
    state: &AppState,
    thread_id: &ThreadId,
) -> Result<Vec<(u64, Value)>, Response> {
    let entries = thread_domain_events(state, thread_id)?;
    Ok(entries
        .into_iter()
        .filter_map(|(event_id, sequence, created_at_ms, event)| {
            let DomainEvent::ThreadRunEvent { run } = event else {
                return None;
            };
            Some((
                sequence,
                thread_event_row(event_id, sequence, created_at_ms, &run),
            ))
        })
        .collect())
}

/// Query of a `threads.timelineTurnSummaryDetails` read.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TurnSummaryDetailsQuery {
    turn_id: Option<String>,
    source_seq_start: Option<String>,
    source_seq_end: Option<String>,
    before_cursor: Option<String>,
}

/// `threads.timelineTurnSummaryDetails`: the rows of one turn, newest page
/// first.
///
/// This is a **filtered view of `threads.timeline`, not a second projection**:
/// the rows are built by the same [`timeline_row_for_event`], so a row this
/// route answers is byte-identical to the same row in the timeline it came
/// from. What differs is the selection — the caller names a turn and a source
/// sequence range — and the paging, which walks *backwards* because the UI
/// expands a collapsed turn from its newest summary row towards its oldest.
///
/// `beforeCursor` is the `rows[].id` of the oldest row the caller already has;
/// the response carries every matching row strictly before it, capped at the
/// same segment limit the timeline uses, plus `olderCursor` when more remain.
/// `historySnapshot` is always `null`: loom has no snapshot of a turn's
/// pre-compaction history, and answering a fabricated one would be worse than
/// the null the contract allows.
async fn thread_turn_summary_details(
    State(state): State<AppState>,
    Path(raw_thread_id): Path<String>,
    Query(query): Query<TurnSummaryDetailsQuery>,
) -> Response {
    let thread_id = match parse_thread_id(&raw_thread_id) {
        Ok(thread_id) => thread_id,
        Err(response) => return response,
    };
    if let Err(response) = public_thread_or_response(&state, &thread_id) {
        return response;
    }
    let source_start = match parse_query_sequence(query.source_seq_start.as_ref(), "sourceSeqStart")
    {
        Ok(Some(value)) => value,
        Ok(None) => {
            return error_response_with_code(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "sourceSeqStart is required".to_owned(),
            )
        }
        Err(response) => return response,
    };
    let source_end = match parse_query_sequence(query.source_seq_end.as_ref(), "sourceSeqEnd") {
        Ok(Some(value)) => value,
        Ok(None) => {
            return error_response_with_code(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "sourceSeqEnd is required".to_owned(),
            )
        }
        Err(response) => return response,
    };
    if query.turn_id.as_deref().is_none_or(str::is_empty) {
        return error_response_with_code(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "turnId is required".to_owned(),
        );
    }

    let entries = match thread_domain_events(&state, &thread_id) {
        Ok(entries) => entries,
        Err(response) => return response,
    };
    // The row set is the timeline's, filtered to the turn's range. Filtering on
    // the event's own turn scope would drop the user message that opened the
    // turn — it is thread-scoped by construction — which is exactly the row a
    // summary expansion is anchored on.
    let mut rows = entries
        .iter()
        .filter(|(_, sequence, _, _)| *sequence >= source_start && *sequence <= source_end)
        .filter_map(|(_event_id, sequence, _created_at_ms, event)| {
            timeline_row_for_event(&thread_id, *sequence, event)
        })
        .collect::<Vec<_>>();

    let before_cursor = query
        .before_cursor
        .as_deref()
        .filter(|value| !value.is_empty());
    if let Some(cursor) = before_cursor {
        // The cursor is a row id from a previous page; the cut is made on the
        // cursor row's own position so a caller that pages twice cannot skip a
        // row that shares its sequence.
        if let Some(cut) = rows
            .iter()
            .position(|row| row.get("id").and_then(Value::as_str) == Some(cursor))
        {
            rows.truncate(cut);
        } else {
            return error_response_with_code(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                format!("beforeCursor {cursor:?} is not a row of turn range {source_start}..{source_end}"),
            );
        }
    }

    // Newest page first: the UI expands from the recent end. `segment_limit`
    // matches the timeline's cap for the same reason — one page is one render.
    const TURN_DETAILS_SEGMENT_LIMIT: usize = 100;
    let has_older = rows.len() > TURN_DETAILS_SEGMENT_LIMIT;
    if rows.len() > TURN_DETAILS_SEGMENT_LIMIT {
        let start = rows.len() - TURN_DETAILS_SEGMENT_LIMIT;
        rows = rows.split_off(start);
    }
    let older_cursor = has_older.then(|| {
        rows.first()
            .and_then(|row| row.get("id").cloned())
            .unwrap_or(Value::Null)
    });

    Json(json!({
        "rows": rows,
        "historySnapshot": Value::Null,
        "olderCursor": older_cursor,
    }))
    .into_response()
}

/// `threads.clearGoal`: clears the goal the thread's log currently shows.
///
/// loom has no goal *entity*: a goal arrives as the contract's
/// `thread/goal/updated` event and is a projection of the thread's run log (the
/// same way the reference UI's `extractThreadTimelineGoal` reads it). Clearing
/// it therefore means publishing the pair of events a client projects —
/// `thread/goal/cleared` — not deleting a row, and that is why this route is
/// implementable at all while [`clear_thread_context`] is not.
///
/// The route is idempotent: clearing a goal that is not set publishes the same
/// event and is the state the caller asked for.
async fn clear_thread_goal(
    State(state): State<AppState>,
    Path(raw_thread_id): Path<String>,
) -> Response {
    let thread_id = match parse_thread_id(&raw_thread_id) {
        Ok(thread_id) => thread_id,
        Err(response) => return response,
    };
    let thread = match public_thread_or_response(&state, &thread_id) {
        Ok(thread) => thread,
        Err(response) => return response,
    };
    let run_id = thread
        .active_run_id
        // A goal is thread-level metadata, so it needs no active run. The run
        // id is only the turn scope on the event; a settled thread scopes the
        // clearing to the thread, which is what `thread/goal/cleared` means.
        .unwrap_or_else(loom_domain::RunId::mint);
    let event = loom_domain::RunEvent::new(
        thread_id.clone(),
        thread.project_id,
        run_id,
        loom_relay::now_ms(),
        loom_domain::ProviderEvent::ThreadGoalCleared {
            provider_thread_id: thread_id.to_string(),
        },
    );
    if let Err(error) = state.publish_domain_event(&DomainEvent::ThreadRunEvent {
        run: Box::new(event),
    }) {
        return error_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string());
    }
    Json(json!({ "ok": true })).into_response()
}

/// `threads.clearContext`: refused, explicitly.
///
/// Clearing a context means emptying the provider's own ACP session memory.
/// loom's provider runs are dispatched statelessly, but the session persists in
/// the ACP agent, so a server-side "clear" would drop loom's records while the
/// provider carried on with the context it still holds — the failure mode the
/// acceptance criteria name.
/// `loom_provider_protocol` has no frame for it, so the route answers bb's
/// `not_configured` at the `501` that code declares rather than a `{ok:true}`
/// for something that did not happen.
async fn clear_thread_context(
    State(state): State<AppState>,
    Path(raw_thread_id): Path<String>,
) -> Response {
    let thread_id = match parse_thread_id(&raw_thread_id) {
        Ok(thread_id) => thread_id,
        Err(response) => return response,
    };
    if let Err(response) = public_thread_or_response(&state, &thread_id) {
        return response;
    }
    error_response_with_code(
        StatusCode::NOT_IMPLEMENTED,
        "not_configured",
        format!(
            "loom's provider protocol has no frame that clears a provider session's context; \
             thread {thread_id} keeps its context"
        ),
    )
}

/// `threads.cancelPlan`: refused, explicitly.
///
/// A plan is the provider's own working state, reported through
/// `turn/plan/updated`. Cancelling it means telling the provider to stop
/// pursuing it, and there is no frame for that — the protocol has dispatch,
/// provision and report and nothing else. Publishing a `turn/plan/updated` that
/// says "cancelled" would change what loom's projection displays while the
/// provider kept executing the plan, which is the silently-wrong answer this
/// route refuses to give.
async fn cancel_thread_plan(
    State(state): State<AppState>,
    Path(raw_thread_id): Path<String>,
) -> Response {
    let thread_id = match parse_thread_id(&raw_thread_id) {
        Ok(thread_id) => thread_id,
        Err(response) => return response,
    };
    if let Err(response) = public_thread_or_response(&state, &thread_id) {
        return response;
    }
    error_response_with_code(
        StatusCode::NOT_IMPLEMENTED,
        "not_configured",
        format!(
            "loom's provider protocol cannot cancel a running plan; stop the thread's turn \
             instead (thread {thread_id})"
        ),
    )
}

/// The thread's current goal, projected from its own run log.
///
/// loom has no goal *entity*: a goal is what the contract's
/// `thread/goal/updated` and `thread/goal/cleared` events say it is, and the
/// reference UI already projects it the same way
/// (`ui/packages/thread-view/src/goal-snapshot-extraction.ts`). Reusing that
/// projection here — rather than adding a stored field — is what makes
/// `threads.clearGoal` work without a second source of truth: clearing
/// publishes `thread/goal/cleared` and the next read reflects it.
///
/// `updatedAt` and `sourceSeq` come from the event row, so a client can order
/// the goal against the rest of the timeline.
fn goal_value(
    _state: &AppState,
    _thread_id: &ThreadId,
    entries: &[(String, u64, u64, DomainEvent)],
) -> Value {
    let mut goal: Option<Value> = None;
    for (_event_id, sequence, created_at_ms, event) in entries {
        let DomainEvent::ThreadRunEvent { run } = event else {
            continue;
        };
        let value = serde_json::to_value(&run.event).expect("ThreadEvent always serializes");
        match value.get("type").and_then(Value::as_str) {
            Some("thread/goal/updated") => {
                goal = Some(json!({
                    "sourceSeq": sequence,
                    "updatedAt": created_at_ms,
                    "objective": value.get("objective"),
                    "status": value.get("status"),
                    "tokenBudget": value.get("tokenBudget"),
                    "tokensUsed": value.get("tokensUsed"),
                    "timeUsedSeconds": value.get("timeUsedSeconds"),
                }));
            }
            Some("thread/goal/cleared") => goal = None,
            _ => {}
        }
    }
    goal.unwrap_or(Value::Null)
}

/// Whether a thread's log currently shows a goal.
///
/// The thread list needs this as a *count* (`activeGoalCount`), and the only
/// place a goal lives is the thread's own run log, so this replays the room.
/// That is the same cost [`thread_search_matches`] already pays for every
/// thread in a list, and it is bounded by the retention window.
///
/// A goal in any status counts, because "the thread has a goal" is what drives
/// the sidebar's goal affordance; the projection a client renders carries the
/// status itself.
fn thread_has_goal(state: &AppState, thread_id: &ThreadId) -> bool {
    let entries = match thread_domain_events(state, thread_id) {
        Ok(entries) => entries,
        Err(_) => return false,
    };
    let mut active = false;
    for (_event_id, _sequence, _created_at_ms, event) in &entries {
        let DomainEvent::ThreadRunEvent { run } = event else {
            continue;
        };
        match run.event.kind() {
            "thread/goal/updated" => active = true,
            "thread/goal/cleared" => active = false,
            _ => {}
        }
    }
    active
}

/// A thread's count of active plans, for `threads.timeline`'s activity row.
///
/// A plan is provider-reported (`turn/plan/updated`) and loom keeps no plan
/// entity, so the honest count is derived from the log exactly as the goal is.
/// A plan that has not been removed by a later `plan/removed`/turn end counts
/// as active.
fn thread_has_active_plan(state: &AppState, thread_id: &ThreadId) -> bool {
    let entries = match thread_domain_events(state, thread_id) {
        Ok(entries) => entries,
        Err(_) => return false,
    };
    let mut active = false;
    for (_event_id, _sequence, _created_at_ms, event) in &entries {
        let DomainEvent::ThreadRunEvent { run } = event else {
            continue;
        };
        match run.event.kind() {
            "turn/plan/updated" => active = true,
            // A settled turn cannot still be in plan mode: the plan was the
            // turn's, and the turn is over.
            "turn/completed" => active = false,
            _ => {}
        }
    }
    active
}

fn create_thread_input_text(input: &[Value]) -> Result<Option<String>, String> {
    if input.is_empty() {
        return Ok(None);
    }
    text_from_queued_input(&Value::Array(input.to_vec())).map(Some)
}

/// Creates a thread and returns it in bb's `threadSchema` shape (`$defs/d7`)
/// with the contract's `201`.
///
/// `projectId` is required. The creation event goes to the project scope so
/// the project list can learn about the new thread. A non-empty initial input
/// then uses the same message -> status -> ACP dispatch path as `threads.send`;
/// this keeps first-turn and follow-up behavior identical.
async fn create_thread(
    State(state): State<AppState>,
    Json(request): Json<CreateThreadRequest>,
) -> Response {
    let initial_content = match create_thread_input_text(&request.input) {
        Ok(content) => content,
        Err(message) => return error_response(StatusCode::BAD_REQUEST, message),
    };
    let (thread, event) = match state.registry.create_thread(
        Some(request.project_id.clone()),
        request.title.clone(),
        request.environment_id(),
        loom_relay::now_ms(),
    ) {
        Ok(result) => result,
        Err(error) => return command_error_response(error),
    };
    if let Err(error) = state.publish_domain_event(&event) {
        return error_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string());
    }
    // Before the initial prompt, so the first turn runs on what the client
    // picked rather than on the server's defaults.
    apply_execution_options(
        &state,
        &thread.id,
        request.provider_id.as_deref(),
        request.model.as_deref(),
        request.reasoning_level.as_ref(),
    );
    if let Some(content) = initial_content {
        if let Err(response) = append_thread_message(&state, &thread.id, MessageRole::User, content)
        {
            return response;
        }
    }
    let current = state.registry.public_thread(&thread.id).unwrap_or(thread);
    (
        StatusCode::CREATED,
        Json(thread_summary_value(&state, &current)),
    )
        .into_response()
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

/// What appending a message did.
pub struct ThreadTurn {
    /// The events the append published, in order.
    events: Vec<PublishedEvent>,
    /// What the dispatch decided. `None` means the message was appended
    /// without starting a turn — the thread already had a run, or its status
    /// does not start one.
    pub outcome: Option<DispatchOutcome>,
}

/// Why a turn could not be appended.
pub enum TurnError {
    /// The domain refused the message.
    Command(CommandError),
    /// The events could not be published.
    Publish(String),
}

impl TurnError {
    /// The reason, for a caller that has no client to answer.
    pub fn reason(&self) -> String {
        match self {
            Self::Command(error) => error.to_string(),
            Self::Publish(message) => message.clone(),
        }
    }

    fn into_response(self) -> Response {
        match self {
            Self::Command(error) => command_error_response(error),
            Self::Publish(message) => error_response(StatusCode::INTERNAL_SERVER_ERROR, message),
        }
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
    append_thread(state, thread_id, role, content)
        .map(|turn| turn.events)
        .map_err(TurnError::into_response)
}

/// The same append, reporting the dispatch outcome and a plain-text reason.
///
/// The HTTP handlers only need the events — a dispatch failure is already on
/// the thread's timeline — but an automation run has to close itself with the
/// same reason, so what the dispatch decided is returned instead of dropped.
pub fn append_thread_message_or_reason(
    state: &AppState,
    thread_id: &ThreadId,
    role: MessageRole,
    content: String,
) -> Result<ThreadTurn, String> {
    append_thread(state, thread_id, role, content).map_err(|error| error.reason())
}

#[allow(clippy::result_large_err)]
fn append_thread(
    state: &AppState,
    thread_id: &ThreadId,
    role: MessageRole,
    content: String,
) -> Result<ThreadTurn, TurnError> {
    let events = state
        .registry
        .post_message(thread_id, role, content.clone(), loom_relay::now_ms())
        .map_err(TurnError::Command)?;
    let published =
        publish_all(state, &events).map_err(|error| TurnError::Publish(error.to_string()))?;

    // A user message into an idle thread moves it to `working`. That is the
    // trigger for dispatch: find a machine and publish a run to its scope
    // through the relay. If no machine exists the dispatcher fails the thread
    // on the spot, so the status change is never left dangling.
    let started = published
        .iter()
        .any(|event| event.event_type == "thread_status_changed");
    let outcome = if started {
        state
            .registry
            .public_thread(thread_id)
            .map(|thread| state.dispatch_thread(&thread, &content))
    } else {
        None
    };
    Ok(ThreadTurn {
        events: published,
        outcome,
    })
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
    /// A worker-chosen identity, so a reconnect updates the same machine
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
/// Idempotent when the body carries an `id`: a worker that reconnects under
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

/// Records a worker heartbeat. Heartbeats are high frequency and deliberately
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

/// Marks a host's worker detached without closing anything on the server.
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
///
/// The personal project is not in it: it is a scope the client is handed
/// separately (see [`sidebar_bootstrap`]), and bb's own list does not carry it.
async fn list_projects(State(state): State<AppState>) -> Json<Vec<Value>> {
    let personal_id = state.registry.personal_project_id();
    Json(
        state
            .registry
            .projects()
            .iter()
            .filter(|project| project.id != personal_id)
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
    /// one, whose path is decided by the worker that provisions it.
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
/// is dispatched to its host in the same request; if the worker is not there
/// yet the request is retained in the host room and replayed on reconnect.
///
/// The path's *existence* is deliberately **not** checked here. The path is on
/// the host's filesystem, which may be another machine, so the worker is the
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
                "no host is available to own the environment; enroll a worker first".into(),
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
/// with no local worker must not make file browsing or host lookups fail. The
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
pub(crate) fn publish_all(
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
pub(crate) fn command_error_response(error: CommandError) -> Response {
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
    use loom_domain::catalog::CatalogThinkingLevel;
    use loom_provider_protocol::ProviderLaunch;
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
    async fn timeline_exposes_a_preflight_failure_as_a_readable_error_row() {
        let state = test_state();
        let app = router(state.clone());
        let (thread, _) = state
            .registry
            .create_thread(
                Some(state.registry.personal_project_id()),
                Some("preflight failure".into()),
                None,
                loom_relay::now_ms(),
            )
            .unwrap();
        state
            .registry
            .post_message(
                &thread.id,
                MessageRole::User,
                "hi".into(),
                loom_relay::now_ms(),
            )
            .unwrap();
        let thread = state.registry.thread(&thread.id).unwrap();
        assert!(matches!(
            state.dispatch_thread(&thread, "hi"),
            crate::runs::DispatchOutcome::NoEnvironment { .. }
        ));

        let response = get(&app, &format!("/api/v1/threads/{}/timeline", thread.id)).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_json(response).await;
        let contract = loom_contract::Contract::load();
        assert_b1_response(&contract, "threads.timeline", "GET", &body);
        let rows = body["rows"].as_array().unwrap();
        assert!(rows
            .iter()
            .all(|row| row["title"] != "Timeline projection failed"));
        let error = rows
            .iter()
            .find(|row| row["systemKind"] == "error")
            .expect("the timeline contains the preflight diagnostic");
        assert_eq!(error["kind"], "system");
        assert_eq!(
            error["title"],
            "thread has no environment bound; bind one before dispatching"
        );
        assert_eq!(error["status"], "error");
        state.shutdown();
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

    fn catalog_model_fixture(
        id: &str,
        name: &str,
        levels: &[&str],
        default: Option<&str>,
    ) -> CatalogModel {
        CatalogModel {
            id: id.to_owned(),
            name: name.to_owned(),
            thinking_levels: levels
                .iter()
                .map(|level| CatalogThinkingLevel {
                    id: (*level).to_owned(),
                    name: (*level).to_owned(),
                    description: None,
                })
                .collect(),
            default_thinking_level: default.map(str::to_owned),
        }
    }

    /// The composer reads the host's real models, with the agent's own ids and
    /// the ladder that belongs to each.
    ///
    /// The levels deliberately include `off` and `minimal`, which `d250` does
    /// not name: the level vocabulary is the agent's, and this route is not
    /// conformance-validated, so they are served rather than translated.
    #[tokio::test]
    async fn execution_options_serve_the_host_agents_own_catalogue() {
        let state = test_state();
        let host_id = HostId::mint();
        state.catalogs.record(
            &host_id,
            "pi",
            ProviderCatalog {
                current_model: Some("mock/fast".into()),
                models: vec![
                    catalog_model_fixture("mock/fast", "Fast", &["off"], Some("off")),
                    catalog_model_fixture("mock/deep", "Deep", &["minimal", "high"], Some("high")),
                ],
            },
        );
        let app = router(state.clone());
        let json = body_json(
            get(
                &app,
                &format!("/api/v1/system/execution-options?hostId={host_id}"),
            )
            .await,
        )
        .await;

        let models = json["models"].as_array().expect("models is an array");
        assert_eq!(models.len(), 2);
        // The value the client sends back is `model`, and it is the agent's id.
        assert_eq!(models[0]["model"], "mock/fast");
        assert_eq!(models[0]["displayName"], "Fast");
        // The catalogue's own current model is the default, exactly once.
        assert_eq!(models[0]["isDefault"], true);
        assert_eq!(models[1]["isDefault"], false);
        // A model with no reasoning shows only `off`.
        assert_eq!(
            models[0]["supportedReasoningEfforts"][0]["reasoningEffort"],
            "off"
        );
        assert_eq!(models[0]["defaultReasoningEffort"], "off");
        // The ladder follows the model: `minimal` is this model's, not the
        // other's, and the model's stated default is kept.
        assert_eq!(
            models[1]["supportedReasoningEfforts"][0]["reasoningEffort"],
            "minimal"
        );
        assert_eq!(models[1]["defaultReasoningEffort"], "high");
        assert_eq!(
            json["selectedOnlyModels"].as_array().map(Vec::len),
            Some(2),
            "the catalogue is mirrored, and the client filters the duplicate"
        );
        state.shutdown();
    }

    /// A query that names a host the server has no catalogue for never borrows
    /// another host's models, and a query that names no host gets the most
    /// recent report — which is what a fresh install's first composer call is.
    #[tokio::test]
    async fn execution_options_resolve_the_catalogue_by_host() {
        let state = test_state();
        let known = HostId::mint();
        state.catalogs.record(
            &known,
            "pi",
            ProviderCatalog {
                current_model: Some("mock/only".into()),
                models: vec![catalog_model_fixture("mock/only", "Only", &["off"], None)],
            },
        );
        let app = router(state.clone());

        let stranger = HostId::mint();
        let named = body_json(
            get(
                &app,
                &format!("/api/v1/system/execution-options?hostId={stranger}"),
            )
            .await,
        )
        .await;
        assert_eq!(
            named["models"][0]["displayName"], "Default",
            "an unknown host falls back rather than answering with another machine's models"
        );

        let unnamed = body_json(get(&app, "/api/v1/system/execution-options").await).await;
        assert_eq!(unnamed["models"][0]["model"], "mock/only");
        state.shutdown();
    }

    /// A host that has reported nothing yet still gets a usable picker.
    #[tokio::test]
    async fn execution_options_fall_back_when_no_catalogue_exists() {
        let state = test_state();
        let app = router(state.clone());
        let json = body_json(get(&app, "/api/v1/system/execution-options").await).await;
        assert_eq!(json["models"][0]["displayName"], "Default");
        assert_eq!(
            json["models"][0]["supportedReasoningEfforts"][0]["reasoningEffort"],
            "medium"
        );
        state.shutdown();
    }

    fn provider_spec(name: &str, launch: ProviderLaunch, command: &str) -> ProviderSpec {
        ProviderSpec {
            name: name.to_owned(),
            launch,
            command: command.to_owned(),
            args: Vec::new(),
            cwd: None,
        }
    }

    /// A branded provider carries the vendor's light/dark ink, and an unbranded
    /// one omits the object entirely: the client's schema is `optional()` and
    /// A branded provider carries bb's own light/dark ink and hints, an agent
    /// with no colour keeps its hints and drops the tint, and an unknown id gets
    /// neither — the client's schema is `optional()`, so the object is only sent
    /// when there is something true to put in it.
    #[test]
    fn a_branded_provider_carries_bb_own_colours() {
        let logo_url = |id: &str| {
            format!(
                "/api/v1/system/providers/{id}/logo?h={}",
                crate::b10::provider_mark_hash(id)
            )
        };

        let branded = provider_info(&provider_spec(
            "opencode",
            ProviderLaunch::AcpStdio,
            "opencode",
        ));
        assert_eq!(branded["strings"]["iconTint"]["light"], "#2563EB");
        assert_eq!(branded["strings"]["iconTint"]["dark"], "#2563EB");
        assert_eq!(branded["strings"]["installUrl"], "https://opencode.ai/docs");
        assert_eq!(
            branded["strings"]["signInHint"],
            "Run `opencode auth login` on the machine to sign in."
        );
        assert_eq!(
            branded["strings"]["expiredHint"],
            "Your OpenCode session expired. Run `opencode auth login`, then reload."
        );
        // The mark is addressed by content, so a client may cache the URL it was
        // handed forever.
        assert_eq!(branded["logoUrl"], logo_url("opencode"));

        // Cursor publishes a real light/dark pair.
        let cursor = provider_info(&provider_spec("cursor", ProviderLaunch::AcpStdio, "cursor"));
        assert_eq!(cursor["strings"]["iconTint"]["light"], "#111827");
        assert_eq!(cursor["strings"]["iconTint"]["dark"], "#F5F5F5");

        // Hermes has a mark and a sign-in command but no colour of its own: it
        // keeps its strings and drops the tint.
        let untinted = provider_info(&provider_spec("hermes", ProviderLaunch::AcpStdio, "hermes"));
        assert_eq!(
            untinted["strings"]["signInHint"],
            "Run `hermes login` on the machine to sign in."
        );
        assert!(untinted["strings"].get("iconTint").is_none());

        // An agent loom knows nothing about still has a mark — the protocol's —
        // but nothing true to say about signing it in.
        let unknown = provider_info(&provider_spec("nope", ProviderLaunch::AcpStdio, "nope"));
        assert!(unknown.get("strings").is_none());
        assert_eq!(unknown["logoUrl"], logo_url("nope"));
    }

    /// A second configured agent is advertised, catalogued separately, and
    /// served only its own models.
    #[tokio::test]
    async fn each_configured_provider_serves_its_own_catalogue() {
        let state = AppState::build(AppConfig {
            providers: vec![
                provider_spec("pi", ProviderLaunch::AcpEmbeddedPi, "pi"),
                provider_spec("codex", ProviderLaunch::AcpStdio, "codex"),
            ],
            ..AppConfig::default()
        })
        .unwrap();
        let host_id = HostId::mint();
        state.catalogs.record(
            &host_id,
            "pi",
            ProviderCatalog {
                current_model: Some("pi/one".into()),
                models: vec![catalog_model_fixture(
                    "pi/one",
                    "Pi One",
                    &["off"],
                    Some("off"),
                )],
            },
        );
        state.catalogs.record(
            &host_id,
            "codex",
            ProviderCatalog {
                current_model: Some("codex/two".into()),
                models: vec![catalog_model_fixture(
                    "codex/two",
                    "Codex Two",
                    &["minimal"],
                    Some("minimal"),
                )],
            },
        );
        let app = router(state.clone());

        let providers = body_json(get(&app, "/api/v1/system/providers").await).await;
        let ids: Vec<&str> = providers
            .as_array()
            .expect("providers is an array")
            .iter()
            .filter_map(|provider| provider["id"].as_str())
            .collect();
        assert_eq!(ids, vec!["pi", "codex"]);

        let listed = body_json(
            get(
                &app,
                &format!("/api/v1/system/execution-options?hostId={host_id}"),
            )
            .await,
        )
        .await;
        assert_eq!(
            listed["providers"].as_array().map(Vec::len),
            Some(2),
            "the picker draws its provider tabs from this list"
        );
        assert_eq!(
            listed["models"][0]["model"], "pi/one",
            "a query that names no provider gets the default one"
        );

        let codex = body_json(
            get(
                &app,
                &format!("/api/v1/system/execution-options?hostId={host_id}&providerId=codex"),
            )
            .await,
        )
        .await;
        assert_eq!(
            codex["models"].as_array().map(Vec::len),
            Some(1),
            "only the named provider's models"
        );
        assert_eq!(codex["models"][0]["model"], "codex/two");
        assert_eq!(codex["models"][0]["displayName"], "Codex Two");

        let unknown = body_json(
            get(
                &app,
                &format!("/api/v1/system/execution-options?hostId={host_id}&providerId=nope"),
            )
            .await,
        )
        .await;
        assert_eq!(
            unknown["models"][0]["model"], "pi/one",
            "a stale provider id falls back to the default rather than failing the picker"
        );
        state.shutdown();
    }

    /// The composer sends what it picked with the thread it creates, and the
    /// thread records it before the first turn dispatches — the dispatch reads
    /// the thread's record, not the request.
    #[tokio::test]
    async fn creating_a_thread_records_the_composers_execution_options() {
        let state = test_state();
        let app = router(state.clone());
        let project_id = state.registry.personal_project_id().to_string();

        let created = body_json(
            post(
                &app,
                "/api/v1/threads",
                serde_json::json!({
                    "projectId": project_id,
                    "origin": "app",
                    "input": [],
                    "environment": { "type": "project-default" },
                    "providerId": "pi",
                    "model": "mock/deep",
                    "reasoningLevel": "high",
                }),
            )
            .await,
        )
        .await;

        let thread_id = created["id"].as_str().unwrap().parse::<ThreadId>().unwrap();
        let thread = state
            .registry
            .thread(&thread_id)
            .expect("the thread exists");
        assert_eq!(thread.provider_id.as_deref(), Some("pi"));
        assert_eq!(thread.model.as_deref(), Some("mock/deep"));
        assert_eq!(
            thread
                .reasoning_level
                .as_ref()
                .map(ToString::to_string)
                .as_deref(),
            Some("high")
        );
        assert_eq!(
            created["providerId"], "pi",
            "the created thread's summary reports the agent it will run on"
        );
        state.shutdown();
    }

    /// A turn's options travel with the prompt and are recorded on the thread
    /// before the run dispatches, for the same reason.
    #[tokio::test]
    async fn sending_a_message_records_its_execution_options() {
        let state = test_state();
        let app = router(state.clone());
        let project_id = state.registry.personal_project_id().to_string();
        let created = body_json(
            post(
                &app,
                "/api/v1/threads",
                serde_json::json!({
                    "projectId": project_id,
                    "origin": "app",
                    "input": [],
                    "environment": { "type": "project-default" },
                }),
            )
            .await,
        )
        .await;
        let thread_id = created["id"].as_str().unwrap().parse::<ThreadId>().unwrap();

        let response = post(
            &app,
            &format!("/api/v1/threads/{thread_id}/send"),
            serde_json::json!({
                "input": [{ "type": "text", "text": "hi" }],
                "mode": "auto",
                "model": "mock/two",
                "reasoningLevel": "minimal",
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK, "the send is accepted");

        let thread = state
            .registry
            .thread(&thread_id)
            .expect("the thread exists");
        assert_eq!(thread.model.as_deref(), Some("mock/two"));
        assert_eq!(
            thread
                .reasoning_level
                .as_ref()
                .map(ToString::to_string)
                .as_deref(),
            Some("minimal")
        );
        state.shutdown();
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

        // A valid send still works after the rejections. The create request's
        // initial input already started a turn, so `auto` honestly queues this
        // follow-up while that turn is active.
        let response = post(
            &app,
            &send_path,
            serde_json::json!({
                "input": [{ "type": "text", "text": "hello" }],
                "mode": "auto"
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

        // The seeded personal project is *not* listed: it is a scope, which the
        // client reads from `personalProject` and addresses by the reserved id
        // rather than a minted one.
        let listed = body_json(get(&app, "/api/v1/projects").await).await;
        let ids: Vec<&str> = listed
            .as_array()
            .unwrap()
            .iter()
            .map(|project| project["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, vec![project_id.as_str()]);
        assert_eq!(
            state.registry.personal_project_id().to_string(),
            "proj_personal"
        );

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
    async fn environment_provider_catalog_is_host_and_source_aware() {
        let state = test_state();
        let (connected, _) = state
            .registry
            .enroll_host(None, "connected".into(), loom_relay::now_ms())
            .unwrap();
        let (disconnected, _) = state
            .registry
            .enroll_host(None, "disconnected".into(), loom_relay::now_ms())
            .unwrap();
        state
            .registry
            .mark_host_disconnected(&disconnected.id, loom_relay::now_ms())
            .unwrap();
        let app = router(state.clone());

        let response = get(
            &app,
            &format!(
                "/api/v1/system/environment-providers?projectId={}",
                state.registry.personal_project_id()
            ),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_json(response).await;
        let providers = body["providers"].as_array().unwrap();
        let personal = providers
            .iter()
            .find(|provider| provider["id"] == PERSONAL_WORKSPACE_PROVIDER_ID)
            .unwrap();
        assert_eq!(
            personal["machineAvailability"][connected.id.to_string()]["status"],
            "available"
        );
        assert_eq!(
            personal["machineAvailability"][disconnected.id.to_string()]["status"],
            "unavailable"
        );
        let checkout = providers
            .iter()
            .find(|provider| provider["id"] == PROJECT_CHECKOUT_PROVIDER_ID)
            .unwrap();
        assert_eq!(
            checkout["machineAvailability"][connected.id.to_string()]["status"],
            "unavailable"
        );

        let project = body_json(
            post(
                &app,
                "/api/v1/projects",
                serde_json::json!({
                    "name": "work",
                    "source": {
                        "type": "local_path",
                        "hostId": connected.id.to_string(),
                        "path": "/srv/work"
                    }
                }),
            )
            .await,
        )
        .await;
        let response = get(
            &app,
            &format!(
                "/api/v1/system/environment-providers?projectId={}",
                project["id"].as_str().unwrap()
            ),
        )
        .await;
        let body = body_json(response).await;
        let checkout = body["providers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|provider| provider["id"] == PROJECT_CHECKOUT_PROVIDER_ID)
            .unwrap();
        assert_eq!(
            checkout["machineAvailability"][connected.id.to_string()]["status"],
            "available"
        );

        state.shutdown();
    }

    #[tokio::test]
    async fn creating_a_thread_with_input_dispatches_the_first_turn() {
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
            "/api/v1/threads",
            serde_json::json!({
                "projectId": state.registry.personal_project_id().to_string(),
                "origin": "app",
                "input": [{ "type": "text", "text": "first turn", "mentions": [] }],
                "environment": {
                    "type": "reuse",
                    "environmentId": environment.id.to_string()
                }
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::CREATED);
        let body = body_json(response).await;
        assert_eq!(body["status"], "active");
        let thread_id = body["id"].as_str().unwrap();
        let stored = stored_events(&state, &Scope::Thread(thread_id.to_owned()));
        assert_eq!(stored.len(), 2);
        assert_eq!(stored[0]["type"], "thread_message_added");
        assert_eq!(stored[0]["message"]["content"], "first turn");
        assert_eq!(stored[1]["type"], "thread_status_changed");
        assert!(state.runs.for_thread(&thread_id.parse().unwrap()).is_some());

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
    async fn a_server_with_no_worker_stays_up_and_reports_no_primary_host() {
        let state = test_state();
        let app = router(state.clone());

        // Health must answer whether or not any worker ever connected.
        let health = get(&app, "/health").await;
        assert_eq!(health.status(), StatusCode::OK);
        assert_eq!(body_json(health).await["status"], "ok");

        // The primary-host lookup degrades to an explicit "no host", never an
        // error, so file browsing is not stranded on an absent local worker.
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

        // The request is in the host room, ready for a reconnecting worker.
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
    async fn the_router_fallback_serves_the_embedded_app() {
        // The app is compiled into the binary, so every server has a client on
        // the same origin as its API — there is nothing to configure and no
        // second artifact to install.
        let app = router(test_state());

        let response = get(&app, "/").await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert!(String::from_utf8_lossy(&body).contains("/assets/"));

        // A client route is the same document; an API typo is not.
        let client_route = get(&app, "/threads/thr_1").await;
        assert_eq!(client_route.status(), StatusCode::OK);
        let api = get(&app, "/api/v1/typo").await;
        assert_eq!(api.status(), StatusCode::NOT_FOUND);
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
    async fn the_install_version_route_reports_the_protocol_the_worker_compares() {
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
        let bytes = b"a stand-in for the worker binary";
        std::fs::write(dir.path().join("loom-worker"), bytes).unwrap();
        let app = router(state_with_artifacts(dir.path()));

        let response = get(&app, "/install/loom-worker").await;
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
        let bytes = b"an unchanged worker binary";
        std::fs::write(dir.path().join("loom-worker"), bytes).unwrap();
        let app = router(state_with_artifacts(dir.path()));
        let digest = crate::artifacts::sha256_hex(bytes);

        let response = app
            .clone()
            .oneshot(
                Request::get("/install/loom-worker")
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
                Request::get("/install/loom-worker")
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
        std::fs::write(dir.path().join("loom-worker"), b"the local binary").unwrap();
        let app = router(state_with_artifacts(dir.path()));

        // The unnamed binary answers only for this server's own triple.
        let other = if crate::TARGET == "aarch64-unknown-linux-musl" {
            "x86_64-unknown-linux-musl"
        } else {
            "aarch64-unknown-linux-musl"
        };
        let response = get(&app, &format!("/install/loom-worker?target={other}")).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body = body_json(response).await;
        assert_eq!(body["code"], "not_found");
        assert!(
            body["message"].as_str().unwrap().contains(other),
            "the message should name the target: {body}"
        );

        // A target that is not a triple at all is the client's mistake.
        let response = get(&app, "/install/loom-worker?target=..%2F..%2Fetc%2Fpasswd").await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(body_json(response).await["code"], "invalid_request");
    }
}
