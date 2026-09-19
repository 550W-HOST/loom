//! Batch B10: server settings, themes and UI preferences.
//!
//! These handlers own server-local configuration state. Settings are persisted
//! in the domain snapshot and publish typed public cache invalidations; active
//! runs remain owned by the run registry and provider session on the worker.

#![allow(clippy::result_large_err)]

use axum::body::Body;
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

/// The pi mark, copied byte for byte from the vendor's own
/// `https://pi.dev/logo-auto.svg`.
///
/// Serving an invented image would misrepresent the provider, so the asset is
/// the vendor's file rather than a hand-drawn stand-in. The client masks
/// `logoUrl` and fills it with the current text colour, so the three brand
/// fills collapse to a silhouette at render time.
const PI_LOGO_SVG: &[u8] = include_bytes!("../assets/pi.svg");

/// `system.providerLogo`.
///
/// Every provider the control plane offers has a mark, because the picker's
/// provider tabs render the icon and nothing else: a URL that 404s is an
/// invisible tab, not a missing decoration. Only an id that is not offered at
/// all is a 404.
pub async fn provider_logo(
    State(state): State<AppState>,
    AxumPath(provider_id): AxumPath<String>,
) -> Response {
    let Some(spec) = state.provider_spec_by_id(&provider_id) else {
        return api_error(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("provider {provider_id:?} is not offered"),
        );
    };
    let svg = if spec.name == "pi" {
        PI_LOGO_SVG.to_vec()
    } else {
        provider_monogram_svg(&crate::http::provider_display_name(&spec.name)).into_bytes()
    };
    let mut response = Response::new(Body::from(svg));
    response.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("image/svg+xml"),
    );
    response
}

/// The mark for an agent that has no artwork loom can serve.
///
/// Generated rather than borrowed: loom does not ship third-party brand marks,
/// and an agent discovered on a machine must still be recognisable. The initials
/// come from the display name so `OMP` and `OpenCode` do not collapse into the
/// same letter, and they are drawn as a silhouette because the client masks this
/// image and fills it with the surrounding text colour.
fn provider_monogram_svg(display_name: &str) -> String {
    let initials = monogram_initials(display_name);
    let initials = if initials.is_empty() {
        "?".to_owned()
    } else {
        escape_xml(&initials)
    };
    // One glyph is drawn larger than two, so a monogram fills the tab either way.
    let font_size = if initials.chars().count() > 1 {
        300
    } else {
        420
    };
    format!(
        concat!(
            r##"<?xml version="1.0" encoding="UTF-8"?>"##,
            r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 800 800">"##,
            r##"<text x="400" y="400" fill="#000" text-anchor="middle" dominant-baseline="central" "##,
            r##"font-family="system-ui, -apple-system, Segoe UI, Roboto, Helvetica, Arial, sans-serif" "##,
            r##"font-size="{}" font-weight="600">{}</text></svg>"##,
        ),
        font_size, initials
    )
}

/// The one or two letters a provider is recognised by.
///
/// The letters have to distinguish the agents that actually share a first
/// letter: `OMP` and `OpenCode` are one letter apart, not two spellings of `O`.
/// So a single word contributes its own capitals when it has two — the shape the
/// author chose — and its first two letters otherwise, while several words
/// contribute one letter each.
fn monogram_initials(display_name: &str) -> String {
    let words: Vec<&str> = display_name.split_whitespace().collect();
    let initials: String = match words.as_slice() {
        [] => String::new(),
        [word] => {
            let capitals: String = word
                .chars()
                .filter(|letter| letter.is_uppercase())
                .take(2)
                .collect();
            if capitals.chars().count() == 2 {
                capitals
            } else {
                word.chars().take(2).collect()
            }
        }
        _ => words
            .iter()
            .filter_map(|word| word.chars().next())
            .take(2)
            .collect(),
    };
    initials.to_uppercase()
}

/// Escapes the characters that would end a text node or an attribute value.
///
/// A provider id reaches this through a host's report, so it is input, not a
/// constant: an id containing `<` must not be able to break the document.
fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
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

    let configured = state.provider_spec().name;
    let provider_ids = query
        .provider_id
        .map(|provider_id| vec![provider_id])
        .unwrap_or_else(|| vec![configured.clone()]);
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

    /// Every offered provider has a mark, and two agents whose names start with
    /// the same letter do not get the same one — the picker's tabs render the
    /// icon and nothing else, so an invisible or identical tab is a real defect.
    #[tokio::test]
    async fn every_offered_provider_has_a_mark_of_its_own() {
        let state = AppState::build(crate::state::AppConfig::default()).unwrap();
        state.record_host_providers(
            &loom_domain::HostId::mint(),
            ["omp", "opencode", "hermes", "gemini"]
                .iter()
                .map(|name| loom_provider_protocol::ProviderSpec {
                    name: (*name).to_owned(),
                    launch: loom_provider_protocol::ProviderLaunch::AcpStdio,
                    command: (*name).to_owned(),
                    args: Vec::new(),
                    cwd: None,
                })
                .collect(),
        );

        let mut bodies = Vec::new();
        for spec in state.providers() {
            let response = provider_logo(State(state.clone()), AxumPath(spec.name.clone())).await;
            assert_eq!(
                response.status(),
                StatusCode::OK,
                "{} has no mark",
                spec.name
            );
            assert_eq!(
                response.headers()[axum::http::header::CONTENT_TYPE],
                "image/svg+xml",
                "{} is not served as an image",
                spec.name
            );
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            let body = String::from_utf8(bytes.to_vec()).unwrap();
            assert!(
                body.trim_end().ends_with("</svg>"),
                "{} is not an SVG",
                spec.name
            );
            bodies.push(body);
        }
        assert_eq!(bodies.len(), 5);
        for (index, body) in bodies.iter().enumerate() {
            assert!(
                !bodies[..index].contains(body),
                "a provider is drawing another provider's mark"
            );
        }
        // The id the worker reports is input, so an unknown one is a 404 rather
        // than a document built from it.
        let missing = provider_logo(State(state.clone()), AxumPath("nope".into())).await;
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
        state.shutdown();
    }

    /// A provider name is input from a host's report, so a monogram cannot be a
    /// way to inject markup into the served document.
    #[test]
    fn a_monogram_escapes_its_initials() {
        let svg = provider_monogram_svg("<script>");
        assert!(!svg.contains("<script>"), "{svg}");
        assert!(svg.contains("&lt;"), "{svg}");
        assert!(svg.ends_with("</svg>"), "{svg}");
    }

    /// The initials come from the display name, which is what keeps `OMP` and
    /// `OpenCode` from collapsing into one letter.
    #[test]
    fn a_monogram_takes_its_initials_from_the_display_name() {
        let of = |id: &str| provider_monogram_svg(&crate::http::provider_display_name(id));
        assert!(of("omp").contains(">OM<"));
        assert!(of("opencode").contains(">OC<"));
        assert!(of("hermes").contains(">HE<"));
        assert!(of("claude-code").contains(">CC<"));
    }

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
