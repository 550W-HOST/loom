//! HTTP surface.
//!
//! Small on purpose. The interesting surface is the WebSocket; these routes
//! exist so a server can be probed, identified and fed events.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use loom_relay::scope::Scope;
use serde::{Deserialize, Serialize};

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
}
