//! What an agent says it can run: the models it offers, and the thinking-level
//! ladder that belongs to each of them.
//!
//! Every value here is the agent's own. Which levels exist is a property of a
//! model rather than of a session — pi derives the ladder from the model's
//! reasoning flag and thinking-level map — so the catalogue is kept per model
//! and loom carries the ids through unchanged. Nothing in this module decides
//! what a level means.

use serde::{Deserialize, Serialize};

/// The models an agent advertised, and the one its session is on.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderCatalog {
    /// Every model the agent offers, in the order it offered them.
    #[serde(default)]
    pub models: Vec<CatalogModel>,
    /// The model the agent's session is on, when it named one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_model: Option<String>,
}

impl ProviderCatalog {
    /// True when the agent advertised no models at all.
    pub fn is_empty(&self) -> bool {
        self.models.is_empty()
    }
}

/// One model an agent offers, with the ladder that belongs to it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogModel {
    /// The agent's id for the model, which is also the value its `model` option
    /// carries and the value a client sends back to select it.
    pub id: String,
    /// The label the agent gives it.
    pub name: String,
    /// The levels this model supports, in the agent's order.
    #[serde(default)]
    pub thinking_levels: Vec<CatalogThinkingLevel>,
    /// Where the session's level lands on this ladder, when the agent said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_thinking_level: Option<String>,
}

/// One thinking level a model supports.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogThinkingLevel {
    /// The id, which is the value the agent's level option carries.
    pub id: String,
    /// The label the agent gives it.
    pub name: String,
    /// What the level does, when the agent described it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_catalogue_round_trips() {
        let catalog = ProviderCatalog {
            current_model: Some("mock/a".into()),
            models: vec![CatalogModel {
                id: "mock/a".into(),
                name: "mock/A".into(),
                thinking_levels: vec![CatalogThinkingLevel {
                    id: "off".into(),
                    name: "Off".into(),
                    description: Some("No reasoning".into()),
                }],
                default_thinking_level: Some("off".into()),
            }],
        };
        let encoded = serde_json::to_string(&catalog).unwrap();
        assert_eq!(
            serde_json::from_str::<ProviderCatalog>(&encoded).unwrap(),
            catalog
        );
    }

    /// An agent that offers nothing says so with an empty catalogue rather than
    /// a list of one invented model.
    #[test]
    fn an_empty_catalogue_is_empty() {
        assert!(ProviderCatalog::default().is_empty());
    }
}
