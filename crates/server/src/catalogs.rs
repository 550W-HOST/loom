//! The model catalogues hosts have reported, kept per host and provider.
//!
//! A catalogue describes one **agent on one machine**: which models it
//! advertises and the thinking-level ladder that belongs to each. One machine
//! may run several agents — `pi`, `codex`, `claude-code` — and two machines may
//! run different versions of the same one, so the key is the pair and a query
//! never answers one agent's models for another.
//!
//! The store is deliberately in memory. A catalogue is a live fact about a
//! connected agent, read by the worker on every enrollment and refreshed from
//! every session it opens, so a server restart loses nothing that the next
//! worker connection does not report again.

use std::collections::HashMap;
use std::sync::Mutex;

use loom_domain::catalog::ProviderCatalog;
use loom_domain::HostId;

/// Which agent on which machine a catalogue belongs to.
type Key = (HostId, String);

/// What each agent reported, oldest report first.
#[derive(Default)]
pub struct CatalogRegistry {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    /// The most recent catalogue per `(host, provider)`.
    by_key: HashMap<Key, ProviderCatalog>,
    /// Keys in report order, oldest first, so "the newest catalogue for this
    /// provider" is one lookup away. A key re-reporting moves to the back
    /// rather than appearing twice.
    order: Vec<Key>,
}

impl CatalogRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Records the catalogue one agent on one host just reported.
    ///
    /// An empty catalogue still replaces the previous one: "the agent now
    /// advertises no models" is a fact, and keeping stale models alive because
    /// the new answer was empty would offer choices the agent no longer has.
    pub fn record(&self, host_id: &HostId, provider_id: &str, catalog: ProviderCatalog) {
        let key: Key = (host_id.clone(), provider_id.to_owned());
        let mut inner = self.inner.lock().expect("catalog registry lock");
        if inner.by_key.insert(key.clone(), catalog).is_none() {
            inner.order.push(key);
            return;
        }
        if let Some(position) = inner.order.iter().position(|known| known == &key) {
            let key = inner.order.remove(position);
            inner.order.push(key);
        }
    }

    /// The catalogue one agent on one host most recently reported.
    pub fn get(&self, host_id: &HostId, provider_id: &str) -> Option<ProviderCatalog> {
        self.inner
            .lock()
            .expect("catalog registry lock")
            .by_key
            .get(&(host_id.clone(), provider_id.to_owned()))
            .cloned()
    }

    /// The newest non-empty catalogue `provider_id` reported on any host.
    ///
    /// This is the answer for a caller that named no host — a fresh install's
    /// first composer query carries no environment — so the picker can show
    /// real models before the user has chosen where the thread runs. It is
    /// scoped by provider: another agent's models are not a fallback for this
    /// one.
    pub fn most_recent(&self, provider_id: &str) -> Option<ProviderCatalog> {
        let inner = self.inner.lock().expect("catalog registry lock");
        inner
            .order
            .iter()
            .rev()
            .filter(|(_, provider)| provider == provider_id)
            .find_map(|key| inner.by_key.get(key))
            .filter(|catalog| !catalog.is_empty())
            .cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use loom_domain::catalog::{CatalogModel, CatalogThinkingLevel};

    const PI: &str = "pi";
    const CODEX: &str = "codex";

    fn catalog(model: &str) -> ProviderCatalog {
        ProviderCatalog {
            current_model: Some(model.to_owned()),
            models: vec![CatalogModel {
                id: model.to_owned(),
                name: model.to_owned(),
                thinking_levels: vec![CatalogThinkingLevel {
                    id: "off".into(),
                    name: "Off".into(),
                    description: None,
                }],
                default_thinking_level: Some("off".into()),
            }],
        }
    }

    /// A host's own answer is the one it gets back, even when another host
    /// reported more recently.
    #[test]
    fn a_host_gets_its_own_catalogue() {
        let registry = CatalogRegistry::new();
        let first = HostId::mint();
        let second = HostId::mint();
        registry.record(&first, PI, catalog("a/one"));
        registry.record(&second, PI, catalog("b/two"));
        assert_eq!(
            registry.get(&first, PI).unwrap().current_model.unwrap(),
            "a/one"
        );
        assert_eq!(
            registry.get(&second, PI).unwrap().current_model.unwrap(),
            "b/two"
        );
    }

    /// Two agents on one machine are two catalogues, not one overwriting the
    /// other: this is the whole reason the key is a pair.
    #[test]
    fn each_provider_on_a_host_keeps_its_own_catalogue() {
        let registry = CatalogRegistry::new();
        let host = HostId::mint();
        registry.record(&host, PI, catalog("pi/one"));
        registry.record(&host, CODEX, catalog("codex/two"));
        assert_eq!(
            registry.get(&host, PI).unwrap().current_model.unwrap(),
            "pi/one"
        );
        assert_eq!(
            registry.get(&host, CODEX).unwrap().current_model.unwrap(),
            "codex/two"
        );
    }

    /// A caller that named no host sees the newest answer for the provider it
    /// asked about, which is what a fresh install's first query needs.
    #[test]
    fn the_newest_report_for_a_provider_is_the_fallback() {
        let registry = CatalogRegistry::new();
        let first = HostId::mint();
        let second = HostId::mint();
        registry.record(&first, PI, catalog("a/one"));
        registry.record(&second, CODEX, catalog("codex/two"));
        registry.record(&second, PI, catalog("b/two"));
        assert_eq!(
            registry.most_recent(PI).unwrap().current_model.unwrap(),
            "b/two"
        );
        assert_eq!(
            registry.most_recent(CODEX).unwrap().current_model.unwrap(),
            "codex/two"
        );
        assert!(registry.most_recent("claude-code").is_none());
    }

    /// A host that goes quiet is not the fallback: the newest *report* wins,
    /// and only a non-empty answer counts.
    #[test]
    fn an_empty_report_is_not_offered_as_the_fallback() {
        let registry = CatalogRegistry::new();
        let host = HostId::mint();
        registry.record(&host, PI, ProviderCatalog::default());
        assert!(registry.most_recent(PI).is_none());
        assert!(registry.get(&host, PI).unwrap().is_empty());
    }
}
