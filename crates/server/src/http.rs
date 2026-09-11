//! HTTP surface.
//!
//! Small on purpose. The interesting surface is the WebSocket; these routes
//! exist so a server can be probed, identified and fed events.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use loom_domain::{
    DomainError, DomainEvent, DomainScope, Host, HostId, MessageRole, Thread, ThreadId,
};
use loom_relay::scope::Scope;
use serde::{Deserialize, Serialize};

use crate::domain_state::CommandError;
use crate::state::AppState;
use crate::ws;
use crate::PROTOCOL_VERSION;

/// Builds the router for a wired-up state.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/api/v1/version", get(version))
        .route("/api/v1/publish", post(publish))
        .route("/api/v1/replay", get(replay))
        .route("/api/v1/threads", post(create_thread))
        .route("/api/v1/threads/{id}/messages", post(post_thread_message))
        .route("/api/v1/hosts", get(list_hosts).post(register_host))
        .route("/api/v1/hosts/primary", get(primary_host))
        .route("/api/v1/hosts/{id}/heartbeat", post(host_heartbeat))
        .route("/api/v1/hosts/{id}/disconnect", post(disconnect_host))
        .route("/ws", get(ws::client_socket))
        .with_state(state)
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
}

async fn health(State(state): State<AppState>) -> Response {
    let retained_events = match state.relay.retained() {
        Ok(count) => count,
        Err(error) => {
            return error_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string());
        }
    };
    Json(HealthResponse {
        status: "ok",
        protocol_version: PROTOCOL_VERSION,
        node_id: state.relay.origin().to_string(),
        uptime_ms: state.uptime_ms(),
        readers: state.pump.reader_count(),
        retained_events,
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
    (status, Json(serde_json::json!({ "error": message }))).into_response()
}

/// Body of a create-thread request.
#[derive(Clone, Debug, Deserialize)]
pub struct CreateThreadRequest {
    /// The owning project. Omitted means the server's personal project.
    pub project_id: Option<loom_domain::ProjectId>,
    /// Optional display title.
    pub title: Option<String>,
}

/// A created thread and the event that announced it.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct CreateThreadResponse {
    /// The new thread, in status `idle`.
    pub thread: Thread,
    /// The `thread_created` event, published to `project:{project_id}`.
    pub event_id: String,
}

/// Creates a thread.
///
/// The event goes to the project scope, not the (brand new, unsubscribable)
/// thread scope: it is the project's thread list that has to learn about it.
async fn create_thread(
    State(state): State<AppState>,
    Json(request): Json<CreateThreadRequest>,
) -> Response {
    match state
        .registry
        .create_thread(request.project_id, request.title, loom_relay::now_ms())
    {
        Ok((thread, event)) => match state.publish_domain_event(&event) {
            Ok(envelope) => Json(CreateThreadResponse {
                thread,
                event_id: envelope.event_id.to_string(),
            })
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
    match state.registry.post_message(
        &thread_id,
        request.role,
        request.content,
        loom_relay::now_ms(),
    ) {
        Ok(events) => match publish_all(&state, &events) {
            Ok(published) => Json(PostMessageResponse {
                thread_id,
                events: published,
            })
            .into_response(),
            Err(error) => error_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
        },
        Err(error) => command_error_response(error),
    }
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

/// Every host the server knows, in id order.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct HostListResponse {
    /// Known hosts, connected or not.
    pub hosts: Vec<Host>,
}

async fn list_hosts(State(state): State<AppState>) -> Json<HostListResponse> {
    Json(HostListResponse {
        hosts: state.registry.hosts(),
    })
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
    /// Maximum frames to return, most recent kept. Defaults to 100.
    pub limit: Option<usize>,
    /// Return only events strictly newer than this id. Intended to be the last
    /// id the caller already has.
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
}

/// Replays retained frames for a scope.
///
/// Correct client flow is: subscribe on the socket **first**, then call this.
/// Any frame that arrives live in the meantime is also present here (or is
/// newer than the window), and because every frame carries an [`EventId`] the
/// client drops the duplicate. Fetching first would instead risk missing a
/// frame published between the two calls.
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

    // Read a wider window than requested when a cursor is supplied, so
    // filtering by cursor cannot silently truncate the answer.
    let limit = query.limit.unwrap_or(100).clamp(1, 10_000);
    let read_limit = if since.is_some() {
        limit.saturating_mul(4)
    } else {
        limit
    };

    let mut events = match state.relay.replay_scope(&scope, read_limit) {
        Ok(events) => events,
        Err(error) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    };

    if let Some(since) = since {
        events.retain(|envelope| envelope.event_id > since);
    }
    if events.len() > limit {
        let start = events.len() - limit;
        events = events.split_off(start);
    }

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
    }

    #[tokio::test]
    async fn creating_a_thread_defaults_to_the_personal_project() {
        let state = test_state();
        let app = router(state.clone());

        let response = post(&app, "/api/v1/threads", serde_json::json!({})).await;
        assert_eq!(response.status(), StatusCode::OK);
        let json = body_json(response).await;
        assert_eq!(json["thread"]["status"], "idle");
        assert_eq!(
            json["thread"]["project_id"],
            state.registry.personal_project_id().to_string()
        );
        assert_eq!(json["event_id"].as_str().unwrap().len(), 26);

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
            serde_json::json!({ "project_id": loom_domain::ProjectId::mint().to_string() }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_user_message_publishes_message_then_status_to_the_thread_scope() {
        let state = test_state();
        let app = router(state.clone());
        let created = body_json(post(&app, "/api/v1/threads", serde_json::json!({})).await).await;
        let thread_id = created["thread"]["id"].as_str().unwrap().to_string();

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
        let created = body_json(post(&app, "/api/v1/threads", serde_json::json!({})).await).await;
        let thread_id = created["thread"]["id"].as_str().unwrap().to_string();

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
        assert_eq!(hosts["hosts"].as_array().unwrap().len(), 0);

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
        assert_eq!(hosts["hosts"].as_array().unwrap().len(), 1);

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
        assert_eq!(hosts["hosts"][0]["status"], "disconnected");

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
}
