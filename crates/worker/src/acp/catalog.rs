//! Reading an agent's catalogue out of the session config options it published.
//!
//! The `model` option is the spine: every value is a model, and the ladder that
//! belongs to it rides in that value's `_meta` — the channel ACP reserves for
//! implementation extensions, which is where `pi-acp` publishes it. Nothing here
//! interprets a level id; they are carried through as the agent spelled them.

use agent_client_protocol::schema::v2;
use loom_domain::catalog::{CatalogModel, CatalogThinkingLevel, ProviderCatalog};

use super::session::{MODEL_CONFIG_ID, THOUGHT_LEVEL_CONFIG_ID};

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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
}
