//! Batch B10: server settings, themes and UI preferences.
//!
//! These handlers own server-local configuration state. Settings are persisted
//! in the domain snapshot and publish typed public cache invalidations; active
//! runs remain owned by the run registry and provider session on the daemon.

#![allow(clippy::result_large_err)]

use axum::extract::{Multipart, Path as AxumPath, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::protocol::{PublicChangeKind, PublicEntity, ServerMessage};
use crate::settings::{
    self, ExperimentSettings, GeneralSettings, PreferenceUpdateError, UiPreference,
};
use crate::state::AppState;

fn api_error(status: StatusCode, code: &'static str, message: impl Into<String>) -> Response {
    (
        status,
        Json(json!({ "code": code, "message": message.into() })),
    )
        .into_response()
}

fn persist(state: &AppState) -> Result<(), Response> {
    state.snapshot().map_err(|error| {
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            format!("settings could not be persisted: {error}"),
        )
    })
}

fn invalid_settings(message: impl Into<String>) -> Response {
    api_error(StatusCode::BAD_REQUEST, "invalid_request", message)
}

fn publish_system_change(state: &AppState, change: PublicChangeKind) {
    let message = ServerMessage::Changed {
        entity: PublicEntity::System,
        id: None,
        metadata: None,
        changes: vec![change],
    };
    if let Err(error) = state.publish_public_change(&message) {
        eprintln!("loom-server: could not publish settings invalidation: {error}");
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppearanceRequest {
    theme_id: String,
    favicon_color: String,
}

/// `system.appearance`
pub async fn update_appearance(
    State(state): State<AppState>,
    Json(request): Json<AppearanceRequest>,
) -> Response {
    if !settings::is_known_theme(&request.theme_id) {
        return api_error(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("theme {:?} is not configured", request.theme_id),
        );
    }
    if !settings::is_valid_favicon_color(&request.favicon_color) {
        return invalid_settings("faviconColor is not supported");
    }
    let appearance = state
        .settings
        .set_appearance(request.theme_id, request.favicon_color);
    if let Err(response) = persist(&state) {
        return response;
    }
    publish_system_change(&state, PublicChangeKind::ConfigChanged);
    Json(settings::appearance_value(&appearance)).into_response()
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExperimentRequest {
    changelog_preview: bool,
    mobile_app: bool,
    sidebar_progressive_disclosure: bool,
    timeline_windowing: bool,
}

impl From<ExperimentRequest> for ExperimentSettings {
    fn from(request: ExperimentRequest) -> Self {
        Self {
            changelog_preview: request.changelog_preview,
            mobile_app: request.mobile_app,
            sidebar_progressive_disclosure: request.sidebar_progressive_disclosure,
            timeline_windowing: request.timeline_windowing,
        }
    }
}

/// `system.experiments`
pub async fn update_experiments(
    State(state): State<AppState>,
    Json(request): Json<ExperimentRequest>,
) -> Response {
    let experiments = state.settings.set_experiments(request.into());
    if let Err(response) = persist(&state) {
        return response;
    }
    publish_system_change(&state, PublicChangeKind::ConfigChanged);
    Json(settings::experiments_value(&experiments)).into_response()
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GeneralRequest {
    show_keyboard_hints: bool,
    steer_active_thread_on_enter: bool,
    show_diagnostic_events: bool,
    provider_order: Vec<String>,
    default_provider_id: Option<String>,
    streamer_mode: bool,
    managed_branch_prefix: String,
    #[serde(default)]
    show_unhandled_provider_events: Option<bool>,
}

impl From<GeneralRequest> for GeneralSettings {
    fn from(request: GeneralRequest) -> Self {
        Self {
            show_keyboard_hints: request.show_keyboard_hints,
            steer_active_thread_on_enter: request.steer_active_thread_on_enter,
            show_diagnostic_events: request.show_diagnostic_events,
            provider_order: request.provider_order,
            default_provider_id: request.default_provider_id,
            streamer_mode: request.streamer_mode,
            managed_branch_prefix: request.managed_branch_prefix,
            show_unhandled_provider_events: request.show_unhandled_provider_events,
        }
    }
}

/// `system.generalSettings`
pub async fn update_general(
    State(state): State<AppState>,
    Json(request): Json<GeneralRequest>,
) -> Response {
    let general = state.settings.set_general(request.into());
    if let Err(response) = persist(&state) {
        return response;
    }
    publish_system_change(&state, PublicChangeKind::ConfigChanged);
    Json(settings::general_value(&general)).into_response()
}

/// `system.keyboardSettings`
pub async fn update_keyboard(
    State(state): State<AppState>,
    Json(request): Json<Value>,
) -> Response {
    let Some(keyboard) = request.as_array() else {
        // The contract middleware normally catches this before the handler;
        // keeping the guard here also protects direct handler use in tests.
        return invalid_settings("keyboard settings must be an array");
    };
    let keyboard = state.settings.set_keyboard(keyboard.clone());
    if let Err(response) = persist(&state) {
        return response;
    }
    publish_system_change(&state, PublicChangeKind::ConfigChanged);
    Json(Value::Array(keyboard)).into_response()
}

/// `system.providerLogo`.
pub async fn provider_logo(
    State(state): State<AppState>,
    AxumPath(provider_id): AxumPath<String>,
) -> Response {
    if provider_id != state.provider_spec().name {
        return api_error(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("provider {provider_id:?} is not configured"),
        );
    }
    // Provider logos are optional binary assets. loom has no configured asset
    // store yet, so returning a fake image would misrepresent provider state.
    api_error(
        StatusCode::NOT_IMPLEMENTED,
        "not_configured",
        "provider logos are not configured on this server",
    )
}

/// `system.reloadConfig`.
pub async fn reload_config(State(_state): State<AppState>) -> Json<Value> {
    // Process configuration is immutable after AppState::build. The endpoint
    // is an explicit no-op acknowledgement: it never tears down provider
    // runs or changes their ACP policy mid-flight.
    Json(json!({ "ok": true }))
}

/// `system.resolveTheme`.
pub async fn resolve_theme(
    State(state): State<AppState>,
    AxumPath(theme_id): AxumPath<String>,
) -> Response {
    if !settings::is_known_theme(&theme_id) {
        return api_error(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("theme {theme_id:?} is not configured"),
        );
    }
    let appearance = state.settings.appearance();
    let theme = if appearance.theme_id == theme_id {
        appearance
    } else {
        serde_json::from_value(settings::default_theme_value())
            .expect("default theme response matches AppearanceSettings")
    };
    Json(settings::appearance_value(&theme)).into_response()
}

/// `system.themes`.
pub async fn themes(State(state): State<AppState>) -> Json<Value> {
    Json(json!({
        "dir": "",
        "custom": [],
        "plugins": [],
        "active": settings::appearance_value(&state.settings.appearance()),
    }))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageLimitsQuery {
    provider_id: Option<String>,
    host_id: Option<String>,
}

/// `system.usageLimits`.
pub async fn usage_limits(
    State(state): State<AppState>,
    Query(query): Query<UsageLimitsQuery>,
) -> Response {
    if query.provider_id.as_deref().is_some_and(str::is_empty)
        || query.host_id.as_deref().is_some_and(str::is_empty)
    {
        return invalid_settings("providerId and hostId must not be empty");
    }

    let configured = state.provider_spec().name.as_str();
    let provider_ids = query
        .provider_id
        .map(|provider_id| vec![provider_id])
        .unwrap_or_else(|| vec![configured.to_owned()]);
    let result = provider_ids
        .into_iter()
        .map(|provider_id| {
            let status = if provider_id == configured {
                json!({
                    "status": "error",
                    "message": "usage limits are not configured for this provider",
                    "planLabel": null,
                    "accountEmail": null
                })
            } else {
                json!({ "status": "not_installed" })
            };
            (provider_id, status)
        })
        .collect::<serde_json::Map<_, _>>();
    Json(Value::Object(result)).into_response()
}

/// `system.voiceTranscription`.
pub async fn voice_transcription(
    State(_state): State<AppState>,
    _multipart: Multipart,
) -> Response {
    // The form is accepted at the transport boundary, but no transcription
    // service is configured. Returning 501 is explicit and avoids inventing a
    // transcript or claiming that voice capability is available.
    api_error(
        StatusCode::NOT_IMPLEMENTED,
        "not_configured",
        "voice transcription is not configured on this server",
    )
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UiPreferenceUpdateRequest {
    expected_revision: u64,
    value: Value,
}

fn preference_response(key: &str, preference: UiPreference) -> Response {
    Json(json!({
        "key": key,
        "revision": preference.revision,
        "value": preference.value,
    }))
    .into_response()
}

fn preference_error(error: PreferenceUpdateError, key: &str) -> Response {
    match error {
        PreferenceUpdateError::UnknownKey => api_error(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("UI preference {key:?} is not known"),
        ),
        PreferenceUpdateError::InvalidValue => invalid_settings(format!(
            "value for UI preference {key:?} has the wrong shape"
        )),
        PreferenceUpdateError::RevisionConflict { expected, actual } => api_error(
            StatusCode::CONFLICT,
            "conflict",
            format!(
                "UI preference {key:?} changed at revision {actual}; expected revision {expected}"
            ),
        ),
        PreferenceUpdateError::RevisionExhausted => api_error(
            StatusCode::CONFLICT,
            "conflict",
            format!("UI preference {key:?} revision is exhausted"),
        ),
    }
}

/// `system.uiPreferences`
pub async fn ui_preferences(State(state): State<AppState>) -> Json<Value> {
    Json(json!({
        "preferences": state.settings.ui_preferences(),
    }))
}

/// `system.updateUiPreference`
pub async fn update_ui_preference(
    State(state): State<AppState>,
    AxumPath(key): AxumPath<String>,
    Json(request): Json<UiPreferenceUpdateRequest>,
) -> Response {
    let preference =
        match state
            .settings
            .update_ui_preference(&key, request.expected_revision, request.value)
        {
            Ok(preference) => preference,
            Err(error) => return preference_error(error, &key),
        };
    if let Err(response) = persist(&state) {
        return response;
    }
    publish_system_change(&state, PublicChangeKind::UiPreferencesChanged);
    preference_response(&key, preference)
}

/// `system.resetUiPreference`
pub async fn reset_ui_preference(
    State(state): State<AppState>,
    AxumPath(key): AxumPath<String>,
) -> Response {
    let preference = match state.settings.reset_ui_preference(&key) {
        Ok(preference) => preference,
        Err(error) => return preference_error(error, &key),
    };
    if let Err(response) = persist(&state) {
        return response;
    }
    publish_system_change(&state, PublicChangeKind::UiPreferencesChanged);
    preference_response(&key, preference)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn settings_mutation_publishes_a_typed_system_invalidation() {
        let state = AppState::build(crate::state::AppConfig::default()).unwrap();
        let mut events = state.public_events.subscribe();

        let response = update_appearance(
            State(state.clone()),
            Json(AppearanceRequest {
                theme_id: "default".into(),
                favicon_color: "blue".into(),
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);

        let event = tokio::time::timeout(std::time::Duration::from_secs(2), events.recv())
            .await
            .unwrap()
            .unwrap();
        let crate::pump::PublicRealtimeEvent::Envelope(envelope) = event else {
            panic!("settings success unexpectedly reset public realtime");
        };
        let messages = crate::protocol::public_messages_from_frame(&envelope.payload);
        assert_eq!(
            serde_json::to_value(messages.first().unwrap()).unwrap(),
            json!({
                "type": "changed",
                "entity": "system",
                "changes": ["config-changed"]
            })
        );
        state.shutdown();
    }

    #[test]
    fn usage_limit_fallback_is_not_an_ok_window() {
        let value = json!({
            "pi": {
                "status": "error",
                "message": "usage limits are not configured for this provider",
                "planLabel": null,
                "accountEmail": null
            }
        });
        assert!(loom_contract::shared()
            .validate_response(
                loom_contract::shared()
                    .route_by_id("system.usageLimits")
                    .unwrap(),
                200,
                &value,
            )
            .is_empty());
    }
}
