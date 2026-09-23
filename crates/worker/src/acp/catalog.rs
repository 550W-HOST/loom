//! Reading an agent's catalogue out of the session config options it published.
//!
//! The `model` option is the spine: every value is a model, and the ladder that
//! belongs to it rides in that value's `_meta` — the channel ACP reserves for
//! implementation extensions, which is where `pi-acp` publishes it. Nothing here
//! interprets a level id; they are carried through as the agent spelled them.
//!
//! There are two ways in, and they answer the same question at different
//! moments. [`catalog_from_options`] reads the options a live session already
//! holds, so every turn keeps the catalogue honest for free. [`catalog_from_v1_options`]
//! does the same for ACP v1's equivalent response shape. Both probe paths open
//! a throwaway session purely to ask, which is what lets a fresh install show
//! real models before any run has happened — and, because it is a real
//! handshake, what decides whether an agent discovered on `PATH` is offered.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_client_protocol::schema::{v1, v2, ProtocolVersion};
use agent_client_protocol::{on_receive_request, Agent, Client, ConnectTo, ConnectionTo, Error};
use loom_domain::catalog::{CatalogModel, CatalogThinkingLevel, ProviderCatalog};

use super::session::{
    agent_argv, embedded_agent_factory, set_config_option_v1, Transport, MODEL_CONFIG_ID,
    THOUGHT_LEVEL_CONFIG_ID,
};

/// The `_meta` keys a model's ladder travels under.
const THINKING_LEVELS_META: &str = "thinking_levels";
const DEFAULT_THINKING_LEVEL_META: &str = "default_thinking_level";

/// The agent's catalogue, read from the config options it published.
///
/// An agent that publishes no per-model ladders still gets its models listed:
/// the ladder of the model its session is on is taken from the `thought_level`
/// option, which is the only model such an agent can describe. Another model's
/// ladder is then unknown, and is left empty rather than borrowed from a model
/// that is not it.
pub fn catalog_from_options(options: &[v2::SessionConfigOption]) -> ProviderCatalog {
    let Some(model_option) = select_option(options, MODEL_CONFIG_ID) else {
        return ProviderCatalog::default();
    };
    let session_ladder: Vec<CatalogThinkingLevel> = select_option(options, THOUGHT_LEVEL_CONFIG_ID)
        .map(|select| values(select).into_iter().map(level_from_value).collect())
        .unwrap_or_default();

    ProviderCatalog {
        current_model: Some(model_option.current_value.0.to_string()),
        models: values(model_option)
            .into_iter()
            .map(|value| {
                let is_current = value.value.0.as_ref() == model_option.current_value.0.as_ref();
                let (thinking_levels, default_thinking_level) = match ladder_meta(value) {
                    Some(stated) => stated,
                    None if is_current => (session_ladder.clone(), None),
                    None => (Vec::new(), None),
                };
                CatalogModel {
                    id: value.value.0.to_string(),
                    name: value.name.clone(),
                    thinking_levels,
                    default_thinking_level,
                }
            })
            .collect(),
    }
}

/// The select option published under `config_id`, when it is one.
fn select_option<'a>(
    options: &'a [v2::SessionConfigOption],
    config_id: &str,
) -> Option<&'a v2::SessionConfigSelect> {
    match &options
        .iter()
        .find(|option| option.config_id.0.as_ref() == config_id)?
        .kind
    {
        v2::SessionConfigKind::Select(select) => Some(select),
        _ => None,
    }
}

/// The agent's v1 catalogue, read from the config options it published.
///
/// ACP v1 uses `id` where v2 uses `config_id`, and agents are free to choose
/// the id of an option. The semantic category is therefore the fallback for
/// agents such as omp, which calls its thought-level option `thinking`.
pub fn catalog_from_v1_options(options: &[v1::SessionConfigOption]) -> ProviderCatalog {
    let Some(model_option) = select_option_v1(
        options,
        MODEL_CONFIG_ID,
        v1::SessionConfigOptionCategory::Model,
    ) else {
        return ProviderCatalog::default();
    };
    let session_ladder: Vec<CatalogThinkingLevel> = select_option_v1(
        options,
        THOUGHT_LEVEL_CONFIG_ID,
        v1::SessionConfigOptionCategory::ThoughtLevel,
    )
    .map(|select| {
        values_v1(select)
            .into_iter()
            .map(level_from_v1_value)
            .collect()
    })
    .unwrap_or_default();

    ProviderCatalog {
        current_model: Some(model_option.current_value.0.to_string()),
        models: values_v1(model_option)
            .into_iter()
            .map(|value| {
                let is_current = value.value.0.as_ref() == model_option.current_value.0.as_ref();
                let (thinking_levels, default_thinking_level) = match ladder_meta_v1(value) {
                    Some(stated) => stated,
                    None if is_current => (session_ladder.clone(), None),
                    None => (Vec::new(), None),
                };
                CatalogModel {
                    id: value.value.0.to_string(),
                    name: value.name.clone(),
                    thinking_levels,
                    default_thinking_level,
                }
            })
            .collect(),
    }
}

/// The v1 selector with the preferred id, or the matching semantic category,
/// together with the id the agent actually chose.
fn select_option_v1_with_id<'a>(
    options: &'a [v1::SessionConfigOption],
    preferred_id: &str,
    category: v1::SessionConfigOptionCategory,
) -> Option<(String, &'a v1::SessionConfigSelect)> {
    let option = options
        .iter()
        .find(|option| {
            option.id.0.as_ref() == preferred_id
                && matches!(&option.kind, v1::SessionConfigKind::Select(_))
        })
        .or_else(|| {
            options.iter().find(|option| {
                option.category.as_ref() == Some(&category)
                    && matches!(&option.kind, v1::SessionConfigKind::Select(_))
            })
        })?;
    let v1::SessionConfigKind::Select(select) = &option.kind else {
        return None;
    };
    Some((option.id.0.to_string(), select))
}

/// The v1 selector with the preferred id, or the matching semantic category
/// when the agent chose a provider-specific id.
fn select_option_v1<'a>(
    options: &'a [v1::SessionConfigOption],
    preferred_id: &str,
    category: v1::SessionConfigOptionCategory,
) -> Option<&'a v1::SessionConfigSelect> {
    select_option_v1_with_id(options, preferred_id, category).map(|(_, select)| select)
}

/// Probe each v1 model once so agents whose per-model ladder is exposed only
/// after switching models (such as omp) publish a complete catalogue.
async fn catalog_from_v1_session(
    connection: &ConnectionTo<Agent>,
    session_id: &str,
    options: Vec<v1::SessionConfigOption>,
) -> ProviderCatalog {
    let mut catalog = catalog_from_v1_options(&options);
    let Some((model_config_id, model_select)) = select_option_v1_with_id(
        &options,
        MODEL_CONFIG_ID,
        v1::SessionConfigOptionCategory::Model,
    ) else {
        return catalog;
    };
    let current_model = model_select.current_value.0.to_string();
    let model_ids = values_v1(model_select)
        .into_iter()
        .map(|value| value.value.0.to_string())
        .collect::<Vec<_>>();
    let mut selected_model = current_model.clone();

    for model_id in &model_ids {
        if model_id == &current_model {
            continue;
        }
        let Ok(updated) =
            set_config_option_v1(connection, session_id, &model_config_id, model_id).await
        else {
            continue;
        };
        selected_model = model_id.clone();
        let updated_catalog = catalog_from_v1_options(&updated);
        let Some(updated_model) = updated_catalog
            .models
            .into_iter()
            .find(|model| model.id == *model_id)
        else {
            continue;
        };
        if let Some(model) = catalog
            .models
            .iter_mut()
            .find(|model| model.id == *model_id)
        {
            model.thinking_levels = updated_model.thinking_levels;
            model.default_thinking_level = updated_model.default_thinking_level;
        }
    }

    // Leave the throwaway session in the state it started in. This matters to
    // agents that perform cleanup or emit state while the connection closes.
    if selected_model != current_model {
        let _ =
            set_config_option_v1(connection, session_id, &model_config_id, &current_model).await;
    }
    catalog
}

/// A v1 selector's values, whether the agent grouped them or not.
fn values_v1(select: &v1::SessionConfigSelect) -> Vec<&v1::SessionConfigSelectOption> {
    match &select.options {
        v1::SessionConfigSelectOptions::Ungrouped(values) => values.iter().collect(),
        v1::SessionConfigSelectOptions::Grouped(groups) => groups
            .iter()
            .flat_map(|group| group.options.iter())
            .collect(),
        _ => Vec::new(),
    }
}

/// A v1 level as the agent described it on a selector option.
fn level_from_v1_value(value: &v1::SessionConfigSelectOption) -> CatalogThinkingLevel {
    CatalogThinkingLevel {
        id: value.value.0.to_string(),
        name: value.name.clone(),
        description: value.description.clone(),
    }
}

/// The ladder an ACP v1 agent attached to one model value.
fn ladder_meta_v1(
    value: &v1::SessionConfigSelectOption,
) -> Option<(Vec<CatalogThinkingLevel>, Option<String>)> {
    let meta = value.meta.as_ref()?;
    let levels = meta.get(THINKING_LEVELS_META)?.as_array()?;
    let levels = levels
        .iter()
        .filter_map(|level| {
            Some(CatalogThinkingLevel {
                id: level.get("id")?.as_str()?.to_owned(),
                name: level.get("name")?.as_str()?.to_owned(),
                description: level
                    .get("description")
                    .and_then(|description| description.as_str())
                    .map(str::to_owned),
            })
        })
        .collect();
    let default = meta
        .get(DEFAULT_THINKING_LEVEL_META)
        .and_then(|level| level.as_str())
        .map(str::to_owned);
    Some((levels, default))
}

/// A selector's values, whether the agent grouped them or not.
fn values(select: &v2::SessionConfigSelect) -> Vec<&v2::SessionConfigSelectOption> {
    match &select.options {
        v2::SessionConfigSelectOptions::Ungrouped(values) => values.iter().collect(),
        v2::SessionConfigSelectOptions::Grouped(groups) => groups
            .iter()
            .flat_map(|group| group.options.iter())
            .collect(),
        _ => Vec::new(),
    }
}

/// A level as the agent described it on a level option.
fn level_from_value(value: &v2::SessionConfigSelectOption) -> CatalogThinkingLevel {
    CatalogThinkingLevel {
        id: value.value.0.to_string(),
        name: value.name.clone(),
        description: value.description.clone(),
    }
}

/// The ladder an agent attached to one of its model values, with the level it
/// clamps the session's current choice to.
fn ladder_meta(
    value: &v2::SessionConfigSelectOption,
) -> Option<(Vec<CatalogThinkingLevel>, Option<String>)> {
    let meta = value.meta.as_ref()?;
    let levels = meta.get(THINKING_LEVELS_META)?.as_array()?;
    let levels = levels
        .iter()
        .filter_map(|level| {
            Some(CatalogThinkingLevel {
                id: level.get("id")?.as_str()?.to_owned(),
                name: level.get("name")?.as_str()?.to_owned(),
                description: level
                    .get("description")
                    .and_then(|description| description.as_str())
                    .map(str::to_owned),
            })
        })
        .collect();
    let default = meta
        .get(DEFAULT_THINKING_LEVEL_META)
        .and_then(|level| level.as_str())
        .map(str::to_owned);
    Some((levels, default))
}

/// What asking an agent for its catalogue produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CatalogProbeOutcome {
    /// The agent answered with the models it offers.
    Read(ProviderCatalog),
    /// The probe could not run or could not answer: a missing executable, a
    /// refused handshake, or a deadline. A probe that cannot answer is a
    /// reported failure, never a hang.
    Failed {
        /// Why, verbatim, so it can be shown to a user.
        error: String,
    },
}

/// Asks an ACP agent what it can run, without touching any thread's session.
///
/// The catalogue is only published on a session's config options, so it cannot
/// be read without opening one: the probe opens a throwaway session, reads the
/// options, and closes it again. This is what lets a fresh install — where no
/// run has happened yet — already show the agent's real models.
///
/// A successful read is also the handshake an agent discovered on `PATH` must
/// complete before the worker offers it. An agent that does not publish a model
/// selector still passes the handshake with an empty catalogue.
///
/// `cwd` is the directory the probe session is opened in. It is not part of the
/// answer, but the agent needs a workspace it can use.
pub async fn read_catalog(
    transport: Transport,
    cwd: String,
    budget: Duration,
) -> CatalogProbeOutcome {
    probe_transport(transport, cwd, budget, true).await
}

/// Verifies an agent through `initialize` without opening a persistent session.
///
/// Use this for agents that persist an empty `session/new` and expose no delete
/// method. Their model catalogue is reported on the first real run instead.
pub async fn verify_agent(
    transport: Transport,
    cwd: String,
    budget: Duration,
) -> CatalogProbeOutcome {
    probe_transport(transport, cwd, budget, false).await
}

async fn probe_transport(
    transport: Transport,
    cwd: String,
    budget: Duration,
    open_session: bool,
) -> CatalogProbeOutcome {
    let operation = async {
        match transport {
            Transport::Stdio { command, args } => {
                let argv = agent_argv(&command, &args, &cwd);
                agent_client_protocol::AcpAgent::from_args(argv.clone())
                    .map_err(|error| format!("could not describe the ACP agent: {error}"))?;
                probe(
                    move || {
                        agent_client_protocol::AcpAgent::from_args(argv.clone())
                            .expect("validated ACP agent arguments")
                    },
                    cwd,
                    open_session,
                )
                .await
            }
            // Only `pi` is a child here; the adapter itself is in-process, so
            // there is no argv to build for it.
            Transport::EmbeddedPi { command, args } => {
                if !args.is_empty() {
                    return Err(format!(
                        "the embedded pi-acp transport takes no provider arguments, but the \
                         request supplies {args:?}"
                    ));
                }
                // No turn runs here, so there is nothing for the settle
                // fallback to bound.
                probe(
                    embedded_agent_factory(command, std::time::Duration::ZERO),
                    cwd,
                    open_session,
                )
                .await
            }
        }
    };

    match tokio::time::timeout(budget, operation).await {
        Ok(Ok(catalog)) => CatalogProbeOutcome::Read(catalog),
        Ok(Err(error)) => CatalogProbeOutcome::Failed { error },
        Err(_) => CatalogProbeOutcome::Failed {
            error: format!(
                "the ACP agent did not answer the catalogue probe within {}ms",
                budget.as_millis()
            ),
        },
    }
}

/// Runs the initialize-and-open conversation against a connected agent.
///
/// The handshake is the point and the catalogue is the dividend: both versions
/// are registered, and either may publish config options. An agent that does
/// not publish a model selector still yields an empty catalogue while proving
/// it answers — which is what admission is decided on.
async fn probe<C, F>(
    agent_factory: F,
    cwd: String,
    open_session: bool,
) -> Result<ProviderCatalog, String>
where
    C: ConnectTo<Client>,
    F: FnMut() -> C + Send + 'static,
{
    let state = Arc::new(CatalogProbeState::default());
    let v1_cwd = cwd.clone();
    let result = Client
        .protocol_connector()
        .with_v1({
            let state = Arc::clone(&state);
            move || V1CatalogClient {
                state: Arc::clone(&state),
                cwd: v1_cwd.clone(),
                open_session,
            }
        })
        .with_v2({
            let state = Arc::clone(&state);
            move || V2CatalogClient {
                state: Arc::clone(&state),
                cwd: cwd.clone(),
                open_session,
            }
        })
        .connect_to(agent_factory)
        .await;

    result.map_err(|error| format!("the ACP connection ended: {error}"))?;
    state
        .take()
        .ok_or_else(|| "the ACP agent ended without answering the probe".to_owned())
}

#[derive(Default)]
struct CatalogProbeState {
    catalog: Mutex<Option<ProviderCatalog>>,
}

impl CatalogProbeState {
    fn set(&self, catalog: ProviderCatalog) {
        *self.catalog.lock().expect("catalogue probe result lock") = Some(catalog);
    }

    fn take(&self) -> Option<ProviderCatalog> {
        self.catalog
            .lock()
            .expect("catalogue probe result lock")
            .take()
    }
}

/// The v1 half of the probe.
///
/// v1 agents may publish config options just like v2 agents. When they do, the
/// probe reads the model selector and asks for each model's current ladder; when
/// they do not, opening a session still proves the agent is usable and the
/// catalogue stays empty.
/// Registering this client is what lets an agent that only speaks v1 — Pi
/// itself, under a v1 negotiation — be admitted rather than rejected for
/// answering the version it actually supports.
struct V1CatalogClient {
    state: Arc<CatalogProbeState>,
    cwd: String,
    open_session: bool,
}

impl ConnectTo<Agent> for V1CatalogClient {
    async fn connect_to(self, agent: impl ConnectTo<Client>) -> Result<(), Error> {
        let state = self.state;
        let cwd = self.cwd;
        let open_session = self.open_session;
        Client
            .builder()
            .on_receive_request(
                async move |_request: v1::RequestPermissionRequest, responder, _cx| {
                    let _ = responder.respond(v1::RequestPermissionResponse::new(
                        v1::RequestPermissionOutcome::Cancelled,
                    ));
                    Ok(())
                },
                on_receive_request!(),
            )
            .connect_with(agent, |connection: ConnectionTo<Agent>| async move {
                let initialized =
                    connection
                        .send_request(v1::InitializeRequest::new(ProtocolVersion::V1).client_info(
                            v1::Implementation::new("loom", env!("CARGO_PKG_VERSION")),
                        ))
                        .block_task()
                        .await?;
                if initialized.protocol_version != ProtocolVersion::V1 {
                    return Err(Error::internal_error().data(
                        "the ACP agent negotiated an unsupported protocol version for the \
                         catalogue probe",
                    ));
                }
                if !open_session {
                    state.set(ProviderCatalog::default());
                    return Ok(());
                }
                let created = connection
                    .send_request(v1::NewSessionRequest::new(cwd))
                    .block_task()
                    .await?;
                let options = created.config_options.unwrap_or_default();
                let catalog =
                    catalog_from_v1_session(&connection, &created.session_id.0, options).await;
                // The probe's session is throwaway; dropping the connection at
                // the end of this block is what ends it. The helper restores
                // the initial model before that drop.
                state.set(catalog);
                Ok(())
            })
            .await
    }
}

/// The v2 half of the probe: the same handshake, plus the config options only
/// v2 publishes.
struct V2CatalogClient {
    state: Arc<CatalogProbeState>,
    cwd: String,
    open_session: bool,
}

impl ConnectTo<Agent> for V2CatalogClient {
    async fn connect_to(self, agent: impl ConnectTo<Client>) -> Result<(), Error> {
        let state = self.state;
        let cwd = self.cwd;
        let open_session = self.open_session;
        Client
            .v2()
            .on_receive_request(
                async move |_request: v2::RequestPermissionRequest, responder, _cx| {
                    let _ = responder.respond(v2::RequestPermissionResponse::new(
                        v2::RequestPermissionOutcome::Cancelled,
                    ));
                    Ok(())
                },
                on_receive_request!(),
            )
            .connect_with(agent, |connection: ConnectionTo<Agent>| async move {
                let initialized = connection
                    .send_request(v2::InitializeRequest::new(
                        ProtocolVersion::V2,
                        v2::Implementation::new("loom", env!("CARGO_PKG_VERSION")),
                    ))
                    .block_task()
                    .await?;
                if initialized.protocol_version != ProtocolVersion::V2 {
                    return Err(Error::internal_error().data(
                        "the ACP agent negotiated an unsupported protocol version for the \
                         catalogue probe",
                    ));
                }
                if !open_session {
                    state.set(ProviderCatalog::default());
                    return Ok(());
                }
                let created = connection
                    .send_request(v2::NewSessionRequest::new(cwd))
                    .block_task()
                    .await?;
                let catalog = catalog_from_options(&created.config_options);
                // The probe's session is throwaway: closing it keeps the agent
                // from accumulating one conversation per enrollment.
                let _ = connection
                    .send_request(v2::CloseSessionRequest::new(created.session_id.clone()))
                    .block_task()
                    .await;
                state.set(catalog);
                Ok(())
            })
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::PathBuf;
    fn model_option(
        values: Vec<v2::SessionConfigSelectOption>,
        current: &str,
    ) -> v2::SessionConfigOption {
        v2::SessionConfigOption::select(MODEL_CONFIG_ID, "Model", current.to_string(), values)
    }

    fn level_option(values: &[(&str, &str)]) -> v2::SessionConfigOption {
        v2::SessionConfigOption::select(
            THOUGHT_LEVEL_CONFIG_ID,
            "Thinking",
            values
                .first()
                .map(|(id, _)| *id)
                .unwrap_or("off")
                .to_string(),
            values
                .iter()
                .map(|(id, name)| v2::SessionConfigSelectOption::new(*id, *name))
                .collect::<Vec<_>>(),
        )
    }

    fn model_with_ladder(
        id: &str,
        levels: &[&str],
        default: &str,
    ) -> v2::SessionConfigSelectOption {
        v2::SessionConfigSelectOption::new(id, id).meta(
            json!({
                "thinking_levels": levels
                    .iter()
                    .map(|level| json!({
                        "id": level,
                        "name": level,
                        "description": format!("{level} reasoning"),
                    }))
                    .collect::<Vec<_>>(),
                "default_thinking_level": default,
            })
            .as_object()
            .expect("a static ladder is an object")
            .clone(),
        )
    }

    /// Each model keeps the ladder published beside it, with the default the
    /// agent clamps the session's choice to — not one shared list.
    #[test]
    fn every_model_keeps_its_own_ladder() {
        let options = vec![
            model_option(
                vec![
                    model_with_ladder("mock/reasoning", &["off", "high"], "high"),
                    model_with_ladder("mock/plain", &["off"], "off"),
                ],
                "mock/reasoning",
            ),
            level_option(&[("off", "Off"), ("high", "High")]),
        ];

        let catalog = catalog_from_options(&options);
        assert_eq!(catalog.current_model.as_deref(), Some("mock/reasoning"));
        let ids: Vec<&str> = catalog
            .models
            .iter()
            .map(|model| model.id.as_str())
            .collect();
        assert_eq!(ids, vec!["mock/reasoning", "mock/plain"]);

        let ladder = |id: &str| -> Vec<String> {
            catalog
                .models
                .iter()
                .find(|model| model.id == id)
                .expect("advertised")
                .thinking_levels
                .iter()
                .map(|level| level.id.clone())
                .collect()
        };
        assert_eq!(ladder("mock/reasoning"), vec!["off", "high"]);
        assert_eq!(ladder("mock/plain"), vec!["off"]);
        assert_eq!(
            catalog.models[1].default_thinking_level.as_deref(),
            Some("off")
        );
        assert_eq!(
            catalog.models[0].thinking_levels[1].description.as_deref(),
            Some("high reasoning")
        );
    }

    /// An agent that states no per-model ladders can describe only the model
    /// its session is on, so that is the only one given one.
    #[test]
    fn a_ladder_less_agent_describes_only_its_current_model() {
        let options = vec![
            model_option(
                vec![
                    v2::SessionConfigSelectOption::new("mock/a", "A"),
                    v2::SessionConfigSelectOption::new("mock/b", "B"),
                ],
                "mock/a",
            ),
            level_option(&[("off", "Off"), ("medium", "Medium")]),
        ];

        let catalog = catalog_from_options(&options);
        assert_eq!(catalog.models[0].thinking_levels.len(), 2);
        assert!(catalog.models[1].thinking_levels.is_empty());
        assert_eq!(catalog.models[0].default_thinking_level, None);
    }

    /// No models advertised is an empty catalogue, not an error.
    #[test]
    fn an_agent_without_models_has_an_empty_catalogue() {
        assert_eq!(
            catalog_from_options(&[level_option(&[("off", "Off")])]),
            ProviderCatalog::default()
        );
    }

    /// ACP v1 agents may call the thought-level option something other than
    /// `thought_level`; the category is the stable lookup key.
    #[test]
    fn a_v1_catalogue_uses_the_semantic_thought_level_category() {
        let options = vec![
            v1::SessionConfigOption::select(
                "model",
                "Model",
                "omp/model",
                vec![
                    v1::SessionConfigSelectOption::new("omp/model", "OMP model"),
                    v1::SessionConfigSelectOption::new("omp/other", "Other model"),
                ],
            )
            .category(v1::SessionConfigOptionCategory::Model),
            v1::SessionConfigOption::select(
                "thinking",
                "Thinking",
                "high",
                vec![
                    v1::SessionConfigSelectOption::new("off", "Off"),
                    v1::SessionConfigSelectOption::new("high", "High"),
                ],
            )
            .category(v1::SessionConfigOptionCategory::ThoughtLevel),
        ];

        let catalog = catalog_from_v1_options(&options);

        assert_eq!(catalog.current_model.as_deref(), Some("omp/model"));
        assert_eq!(catalog.models.len(), 2);
        assert_eq!(
            catalog.models[0]
                .thinking_levels
                .iter()
                .map(|level| level.id.as_str())
                .collect::<Vec<_>>(),
            vec!["off", "high"]
        );
        assert!(catalog.models[1].thinking_levels.is_empty());
    }

    /// A v1 agent without a model selector remains a valid probe result, but
    /// has no catalogue to publish.
    #[test]
    fn a_v1_agent_without_models_has_an_empty_catalogue() {
        let options = vec![v1::SessionConfigOption::select(
            "thinking",
            "Thinking",
            "off",
            vec![v1::SessionConfigSelectOption::new("off", "Off")],
        )];

        assert_eq!(
            catalog_from_v1_options(&options),
            ProviderCatalog::default()
        );
    }

    /// Writes an executable ACP agent stub that answers a v2 probe.
    ///
    /// `config_options` is the JSON array `session/new` returns. `new_body` is
    /// the shell body for `session/new`; an empty one leaves the method
    /// unanswered, which is what the deadline test wants.
    fn write_probe_agent(dir: &std::path::Path, config_options: &str, new_body: &str) -> PathBuf {
        let path = dir.join("catalog-probe.sh");
        let script = format!(
            r#"#!/bin/sh
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([^,]*\),"method":.*/\1/p')
  method=$(printf '%s' "$line" | sed -n 's/.*"method":"\([^"]*\)".*/\1/p')
  case "$method" in
    initialize)
      printf '%s\n' '{{"jsonrpc":"2.0","id":'"$id"',"result":{{"protocolVersion":2,"info":{{"name":"fake-v2","version":"1"}},"capabilities":{{"session":{{}}}}}}}}'
      ;;
    session/new)
{new_body}
      ;;
    session/close)
      printf '%s\n' '{{"jsonrpc":"2.0","id":'"$id"',"result":{{}}}}'
      ;;
  esac
done
"#
        );
        // The options array is the one `session/new` answers with; the rest of
        // the template has its own braces, escaped above.
        let script = script.replace("%OPTIONS%", config_options);
        std::fs::write(&path, script).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    }

    /// Writes an executable ACP agent stub that negotiates v1 and changes its
    /// thought-level selector when the model selector is changed.
    fn write_v1_model_probe_agent(
        dir: &std::path::Path,
        options_a: &str,
        options_b: &str,
    ) -> PathBuf {
        let path = dir.join("catalog-v1-probe.sh");
        let script = r#"#!/bin/sh
options_a='__OPTIONS_A__'
options_b='__OPTIONS_B__'
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([^,]*\),"method":.*/\1/p')
  method=$(printf '%s' "$line" | sed -n 's/.*"method":"\([^"]*\)".*/\1/p')
  case "$method" in
    initialize)
      printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"result":{"protocolVersion":1,"info":{"name":"fake-v1","version":"1"},"capabilities":{},"agentInfo":{"name":"fake-v1","version":"1"},"agentCapabilities":{}}}'
      ;;
    session/new)
      printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"result":{"sessionId":"probe-v1","configOptions":'"$options_a"'}}'
      ;;
    session/set_config_option)
      case "$line" in
        *'"value":"mock/b"'*) options="$options_b" ;;
        *) options="$options_a" ;;
      esac
      printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"result":{"configOptions":'"$options"'}}'
      ;;
    session/close)
      printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"result":{}}'
      ;;
  esac
done
"#
        .replace("__OPTIONS_A__", options_a)
        .replace("__OPTIONS_B__", options_b);
        std::fs::write(&path, script).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    }

    fn stdio(agent: &std::path::Path) -> Transport {
        Transport::Stdio {
            command: agent.to_string_lossy().into_owned(),
            args: Vec::new(),
        }
    }

    /// The startup probe opens a session, reads the catalogue the agent
    /// published, and closes it — which is what a fresh install needs, before
    /// any run has happened.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_probe_reads_the_catalogue_from_a_probe_session() {
        let dir = tempfile::tempdir().unwrap();
        let options = vec![
            model_option(
                vec![
                    model_with_ladder("mock/reasoning", &["off", "minimal", "high"], "high"),
                    model_with_ladder("mock/plain", &["off"], "off"),
                ],
                "mock/reasoning",
            ),
            level_option(&[("off", "Off"), ("minimal", "Minimal"), ("high", "High")]),
        ];
        let config_options = serde_json::to_string(&options).unwrap();
        // `%OPTIONS%` is substituted by `write_probe_agent`, because a shell
        // fragment full of JSON braces cannot be built with `format!` directly.
        let new_body = r#"      printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"result":{"sessionId":"probe-1","configOptions":%OPTIONS%}}'"#;
        let agent = write_probe_agent(dir.path(), &config_options, new_body);

        let outcome = read_catalog(
            stdio(&agent),
            dir.path().to_string_lossy().into_owned(),
            Duration::from_secs(10),
        )
        .await;

        match outcome {
            CatalogProbeOutcome::Read(catalog) => {
                assert_eq!(catalog.current_model.as_deref(), Some("mock/reasoning"));
                let ladder = |id: &str| -> Vec<String> {
                    catalog
                        .models
                        .iter()
                        .find(|model| model.id == id)
                        .expect("advertised")
                        .thinking_levels
                        .iter()
                        .map(|level| level.id.clone())
                        .collect()
                };
                assert_eq!(ladder("mock/reasoning"), vec!["off", "minimal", "high"]);
                assert_eq!(ladder("mock/plain"), vec!["off"]);
                assert_eq!(
                    catalog.models[0].default_thinking_level.as_deref(),
                    Some("high")
                );
            }
            other => panic!("expected Read, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn initialize_only_verification_does_not_open_a_v1_session() {
        let dir = tempfile::tempdir().unwrap();
        let agent = dir.path().join("resume-only-agent.sh");
        let script = r#"#!/bin/sh
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([^,]*\),"method":.*/\1/p')
  method=$(printf '%s' "$line" | sed -n 's/.*"method":"\([^\"]*\)".*/\1/p')
  case "$method" in
    initialize)
      printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"result":{"protocolVersion":1,"agentInfo":{"name":"resume-only","version":"1"},"agentCapabilities":{"loadSession":false,"sessionCapabilities":{"resume":{},"list":{},"close":{}}}}}'
      ;;
    session/new)
      touch "$0.created"
      printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"result":{"sessionId":"probe-session"}}'
      ;;
  esac
done
"#;
        std::fs::write(&agent, script).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&agent, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let outcome = verify_agent(
            stdio(&agent),
            dir.path().to_string_lossy().into_owned(),
            Duration::from_secs(10),
        )
        .await;

        assert_eq!(
            outcome,
            CatalogProbeOutcome::Read(ProviderCatalog::default())
        );
        assert!(!PathBuf::from(format!("{}.created", agent.display())).exists());
    }

    /// v1 agents such as omp expose the current model's ladder in `session/new`
    /// and publish the next model's ladder only after `session/set_config_option`.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_v1_probe_discovers_ladders_after_model_switches() {
        let dir = tempfile::tempdir().unwrap();
        let options_a = json!([
            {
                "id": "model",
                "name": "Model",
                "category": "model",
                "type": "select",
                "currentValue": "mock/a",
                "options": [
                    {"value": "mock/a", "name": "A"},
                    {"value": "mock/b", "name": "B"}
                ]
            },
            {
                "id": "thinking",
                "name": "Thinking",
                "category": "thought_level",
                "type": "select",
                "currentValue": "off",
                "options": [
                    {"value": "off", "name": "Off"},
                    {"value": "high", "name": "High"}
                ]
            }
        ])
        .to_string();
        let options_b = json!([
            {
                "id": "model",
                "name": "Model",
                "category": "model",
                "type": "select",
                "currentValue": "mock/b",
                "options": [
                    {"value": "mock/a", "name": "A"},
                    {"value": "mock/b", "name": "B"}
                ]
            },
            {
                "id": "thinking",
                "name": "Thinking",
                "category": "thought_level",
                "type": "select",
                "currentValue": "off",
                "options": [
                    {"value": "off", "name": "Off"},
                    {"value": "low", "name": "Low"},
                    {"value": "max", "name": "Max"}
                ]
            }
        ])
        .to_string();
        let agent = write_v1_model_probe_agent(dir.path(), &options_a, &options_b);
        let outcome = read_catalog(
            stdio(&agent),
            dir.path().to_string_lossy().into_owned(),
            Duration::from_secs(10),
        )
        .await;

        let CatalogProbeOutcome::Read(catalog) = outcome else {
            panic!("expected a readable v1 catalogue, got {outcome:?}");
        };
        let ladder = |id: &str| -> Vec<String> {
            catalog
                .models
                .iter()
                .find(|model| model.id == id)
                .expect("advertised")
                .thinking_levels
                .iter()
                .map(|level| level.id.clone())
                .collect()
        };
        assert_eq!(catalog.current_model.as_deref(), Some("mock/a"));
        assert_eq!(ladder("mock/a"), vec!["off", "high"]);
        assert_eq!(ladder("mock/b"), vec!["off", "low", "max"]);
    }

    /// A probe that cannot answer is a reported failure with the deadline in
    /// it, never a hang: enrollment must not wait on a broken agent.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_probe_that_never_answers_fails_with_its_deadline() {
        let dir = tempfile::tempdir().unwrap();
        // An empty body answers nothing, so `session/new` would hang.
        let agent = write_probe_agent(dir.path(), "[]", "");
        let outcome = read_catalog(
            stdio(&agent),
            dir.path().to_string_lossy().into_owned(),
            Duration::from_millis(500),
        )
        .await;

        match outcome {
            CatalogProbeOutcome::Failed { error } => {
                assert!(
                    error.contains("500ms"),
                    "the deadline is reported, got: {error}"
                );
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }
}
