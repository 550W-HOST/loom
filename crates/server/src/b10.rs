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

/// The mark each provider is drawn with.
///
/// These are loom's port of bb's provider icons — one file per agent, taken from
/// the plugin that declares it upstream (`plugins/provider-*/icons/`) — plus a
/// glyph for Gemini, which loom probes and bb has no entry for.
///
/// Every mark is a single path (or a few) painted with `currentColor`, and tone
/// where a mark has any comes from `fill-opacity` rather than a second colour.
/// That is what makes one file work on both themes: the client masks the image
/// and fills it with the theme's text colour, so the mark *is* the theme's ink,
/// and the brand tint on top of it comes from [`provider_branding`]. Shipping a
/// vendor's full-colour artwork instead would fight the mask — it would flatten
/// to a silhouette anyway, and where the artwork relies on colour to be legible
/// (`oh-my-pi`'s near-white bars, Cursor's solid tile) that silhouette is worse
/// than the drawn glyph.
const PROVIDER_MARKS: &[(&str, &[u8])] = &[
    ("pi", include_bytes!("../assets/pi.svg")),
    ("omp", include_bytes!("../assets/omp.svg")),
    ("hermes", include_bytes!("../assets/hermes.svg")),
    ("opencode", include_bytes!("../assets/opencode.svg")),
    ("cursor", include_bytes!("../assets/cursor.svg")),
    ("codex", include_bytes!("../assets/codex.svg")),
    ("claude-code", include_bytes!("../assets/claude-code.svg")),
    ("gemini", include_bytes!("../assets/gemini.svg")),
];

/// The generic Agent Client Protocol mark.
///
/// It is what bb shows for an ACP agent outside its known list, and what loom
/// shows for an agent discovery found that has no mark of its own: the honest
/// answer is "this is an ACP agent", not an invented logo.
const ACP_MARK_SVG: &[u8] = include_bytes!("../assets/acp.svg");

/// The mark for `provider_id`, falling back to the protocol's own mark.
fn provider_mark(provider_id: &str) -> &'static [u8] {
    PROVIDER_MARKS
        .iter()
        .find(|(id, _)| *id == provider_id)
        .map(|(_, svg)| *svg)
        .unwrap_or(ACP_MARK_SVG)
}

/// The content hash a mark is addressed by.
///
/// bb puts it in `logoUrl` as `?h=`, and serves a matching request as immutable:
/// an icon cannot change without its URL changing, so a client may cache it
/// forever. Sixteen hex characters, the same width bb uses.
pub(crate) fn provider_mark_hash(provider_id: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(provider_mark(provider_id));
    digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// How an agent signs in, and how its mark is tinted when it is drawn large.
pub(crate) struct ProviderBranding {
    /// The command that signs the agent in, as its own CLI spells it.
    pub sign_in_command: &'static str,
    /// The vendor's install page, shown when the agent is missing.
    pub install_url: &'static str,
    /// The mark's ink on a light background, when the agent has a colour.
    pub light: Option<&'static str>,
    /// The mark's ink on a dark background, when the agent has a colour.
    pub dark: Option<&'static str>,
}

/// What loom knows about presenting each agent, in bb's own values.
///
/// The tints are bb's, verbatim, from its agent definitions
/// (`plugins/provider-acp/src/known-agents.ts` and the first-party provider
/// plugins): most agents carry one colour for both themes, Cursor carries a real
/// light/dark pair, and the rest carry none and are drawn in the theme's text
/// colour. The sign-in commands and install pages are the vendors' too, and the
/// client turns the command into the hint it shows.
pub(crate) fn provider_branding(provider_id: &str) -> Option<ProviderBranding> {
    let branding = match provider_id {
        "pi" => ProviderBranding {
            sign_in_command: "pi",
            install_url: "https://pi.dev",
            light: Some("#6D5DFB"),
            dark: Some("#6D5DFB"),
        },
        "omp" => ProviderBranding {
            sign_in_command: "omp login",
            install_url: "https://github.com/can1357/omp",
            light: Some("#9333EA"),
            dark: Some("#9333EA"),
        },
        "opencode" => ProviderBranding {
            sign_in_command: "opencode auth login",
            install_url: "https://opencode.ai/docs",
            light: Some("#2563EB"),
            dark: Some("#2563EB"),
        },
        "cursor" => ProviderBranding {
            sign_in_command: "cursor-agent login",
            install_url: "https://cursor.com/docs/cli/installation",
            light: Some("#111827"),
            dark: Some("#F5F5F5"),
        },
        "codex" => ProviderBranding {
            sign_in_command: "codex",
            install_url: "https://developers.openai.com/codex/cli",
            light: None,
            dark: None,
        },
        "claude-code" => ProviderBranding {
            sign_in_command: "claude",
            install_url: "https://claude.com/claude-code",
            light: Some("#D97757"),
            dark: Some("#D97757"),
        },
        "hermes" => ProviderBranding {
            sign_in_command: "hermes login",
            install_url: "https://hermes-agent.nousresearch.com",
            light: None,
            dark: None,
        },
        "gemini" => ProviderBranding {
            sign_in_command: "gemini",
            install_url: "https://github.com/google-gemini/gemini-cli",
            light: None,
            dark: None,
        },
        _ => return None,
    };
    Some(branding)
}

/// What a logo request asked for.
#[derive(Debug, Deserialize)]
pub struct ProviderLogoQuery {
    /// The content hash the client was given in `logoUrl`.
    h: Option<String>,
}

/// `system.providerLogo`.
///
/// Every offered provider has a mark — the picker's tabs render the icon and
/// nothing else, so a URL that 404s is an invisible tab rather than a missing
/// decoration — and an agent with no mark of its own gets the protocol's. Only
/// an id that is not offered at all is a 404.
pub async fn provider_logo(
    State(state): State<AppState>,
    AxumPath(provider_id): AxumPath<String>,
    Query(query): Query<ProviderLogoQuery>,
) -> Response {
    if state.provider_spec_by_id(&provider_id).is_none() {
        return api_error(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("provider {provider_id:?} is not offered"),
        );
    }
    let mark = provider_mark(&provider_id);
    let mut response = Response::new(Body::from(mark.to_vec()));
    let headers = response.headers_mut();
    headers.insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("image/svg+xml"),
    );
    // A request that names the current mark is immutable; anything else — an
    // unknown hash, or none at all — is answered but not cached, so a client
    // that guessed a URL cannot pin a stale icon.
    headers.insert(
        axum::http::header::CACHE_CONTROL,
        if query.h.as_deref() == Some(provider_mark_hash(&provider_id).as_str()) {
            axum::http::HeaderValue::from_static("public, max-age=31536000, immutable")
        } else {
            axum::http::HeaderValue::from_static("no-store")
        },
    );
    // The mark is a document served from the application's own origin, and a
    // browser renders one as a document when it is opened directly. Loom only
    // ever uses it as a CSS mask, so a script inside one would have no purpose
    // beyond running in that origin: the policy forbids them, while inline
    // styles stay allowed because some marks use them.
    headers.insert(
        axum::http::header::CONTENT_SECURITY_POLICY,
        axum::http::HeaderValue::from_static("default-src 'none'; style-src 'unsafe-inline'"),
    );
    headers.insert(
        axum::http::header::X_CONTENT_TYPE_OPTIONS,
        axum::http::HeaderValue::from_static("nosniff"),
    );
    response
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

    /// Every offered provider is served a mark, the marks are the drawn glyphs
    /// (never a vendor's colour artwork), and a request that names the current
    /// hash is the only one a client may cache.
    #[tokio::test]
    async fn every_offered_provider_is_served_a_mark() {
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
            let response = provider_logo(
                State(state.clone()),
                AxumPath(spec.name.clone()),
                Query(ProviderLogoQuery { h: None }),
            )
            .await;
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
            // An unhashed request still answers — a client may have an old URL —
            // but it must not be cached.
            assert_eq!(
                response.headers()[axum::http::header::CACHE_CONTROL],
                "no-store"
            );
            assert_eq!(
                response.headers()[axum::http::header::CONTENT_SECURITY_POLICY],
                "default-src 'none'; style-src 'unsafe-inline'",
                "a mark served as a document must not be able to run scripts"
            );
            assert_eq!(
                response.headers()[axum::http::header::X_CONTENT_TYPE_OPTIONS],
                "nosniff"
            );
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            let body = String::from_utf8(bytes.to_vec()).unwrap();
            assert_eq!(
                body.as_bytes(),
                provider_mark(&spec.name),
                "{} is not the mark loom ships",
                spec.name
            );
            // The drawn glyphs paint themselves with the theme's text colour;
            // a mark with a fixed fill would be invisible in one of the themes.
            assert!(
                body.contains("currentColor"),
                "{} does not follow the theme",
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

        // The hashed URL a client is given is the immutable one.
        let hash = provider_mark_hash("omp");
        assert_eq!(hash.len(), 16, "bb addresses a mark with 16 hex characters");
        let hashed = provider_logo(
            State(state.clone()),
            AxumPath("omp".into()),
            Query(ProviderLogoQuery {
                h: Some(hash.clone()),
            }),
        )
        .await;
        assert_eq!(
            hashed.headers()[axum::http::header::CACHE_CONTROL],
            "public, max-age=31536000, immutable"
        );
        // A stale or invented hash is answered but never cached.
        let stale = provider_logo(
            State(state.clone()),
            AxumPath("omp".into()),
            Query(ProviderLogoQuery {
                h: Some("0000000000000000".into()),
            }),
        )
        .await;
        assert_eq!(
            stale.headers()[axum::http::header::CACHE_CONTROL],
            "no-store"
        );

        // The id the worker reports is input, so an unknown one is a 404 rather
        // than a document built from it.
        let missing = provider_logo(
            State(state.clone()),
            AxumPath("nope".into()),
            Query(ProviderLogoQuery { h: None }),
        )
        .await;
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
        state.shutdown();
    }

    /// An agent loom discovered but has no glyph for is drawn as an ACP agent
    /// rather than as an invented logo — bb's fallback, kept.
    #[tokio::test]
    async fn an_unknown_agent_gets_the_protocol_mark() {
        let state = AppState::build(crate::state::AppConfig::default()).unwrap();
        state.record_host_providers(
            &loom_domain::HostId::mint(),
            vec![loom_provider_protocol::ProviderSpec {
                name: "some-new-agent".into(),
                launch: loom_provider_protocol::ProviderLaunch::AcpStdio,
                command: "some-new-agent".into(),
                args: Vec::new(),
                cwd: None,
            }],
        );

        let response = provider_logo(
            State(state.clone()),
            AxumPath("some-new-agent".into()),
            Query(ProviderLogoQuery { h: None }),
        )
        .await;
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(bytes.as_ref(), ACP_MARK_SVG);
        assert!(
            provider_branding("some-new-agent").is_none(),
            "an agent with no glyph has no sign-in hint to give either"
        );
        state.shutdown();
    }

    /// Every shipped mark is a usable document that follows the theme, and each
    /// names an id the worker can actually report — a typo here would be a
    /// silently generic icon.
    #[test]
    fn every_shipped_mark_is_a_provider_id_and_a_themed_svg() {
        assert!(!PROVIDER_MARKS.is_empty());
        for (provider_id, svg) in PROVIDER_MARKS {
            assert!(!provider_id.is_empty() && !provider_id.contains(char::is_whitespace));
            let text = std::str::from_utf8(svg).expect("a mark is UTF-8 text");
            assert!(text.contains("<svg"), "{provider_id} is not an SVG");
            assert!(
                text.trim_end().ends_with("</svg>"),
                "{provider_id} is truncated"
            );
            assert!(
                text.contains("currentColor"),
                "{provider_id} would not follow the theme"
            );
            assert!(!text.contains("<script"), "{provider_id} carries a script");
            let listed = PROVIDER_MARKS
                .iter()
                .filter(|(id, _)| id == provider_id)
                .count();
            assert_eq!(listed, 1, "{provider_id} is listed twice");
        }
        assert!(
            String::from_utf8_lossy(ACP_MARK_SVG).contains("currentColor"),
            "the fallback mark must follow the theme too"
        );
    }

    /// Every agent with a glyph names who signs it in and where to get it, its
    /// colours are ones CSS accepts, and its install page is a real link.
    #[test]
    fn every_shipped_mark_has_usable_branding() {
        for (provider_id, _) in PROVIDER_MARKS {
            let branding = provider_branding(provider_id)
                .unwrap_or_else(|| panic!("{provider_id} ships a mark but no branding"));
            assert!(
                !branding.sign_in_command.is_empty(),
                "{provider_id} has no sign-in command"
            );
            assert!(
                branding.install_url.starts_with("https://"),
                "{provider_id}: {} is not a link",
                branding.install_url
            );
            match (branding.light, branding.dark) {
                (None, None) => {}
                (Some(light), Some(dark)) => {
                    for colour in [light, dark] {
                        let digits = colour.strip_prefix('#').unwrap_or_else(|| {
                            panic!("{provider_id}: {colour} is not a hex colour")
                        });
                        assert!(
                            matches!(digits.len(), 3 | 6 | 8)
                                && digits.chars().all(|digit| digit.is_ascii_hexdigit()),
                            "{provider_id}: {colour} is not a hex colour"
                        );
                    }
                }
                _ => panic!("{provider_id} has half a tint pair"),
            }
        }
        assert!(
            provider_branding("some-new-agent").is_none(),
            "an agent loom does not know has no branding either"
        );
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
