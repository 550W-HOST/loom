//! The model catalogues hosts have reported, kept per host.
//!
//! A catalogue describes the agent installed on **one machine**: which models
//! it advertises and the thinking-level ladder that belongs to each. Two hosts
//! may run different agents (or different versions of one), so the server keeps
//! them apart and never answers one host's models for another.
//!
//! The store is deliberately in memory. A catalogue is a live fact about a
//! connected agent, read by the worker on every enrollment and refreshed from
//! every session it opens, so a server restart loses nothing that the next
//! worker connection does not report again.

use std::collections::HashMap;
use std::sync::Mutex;

use loom_domain::catalog::ProviderCatalog;
use loom_domain::HostId;

/// What each host's agent reported, oldest report first.
#[derive(Default)]
pub struct CatalogRegistry {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    /// The most recent catalogue per host.
    by_host: HashMap<HostId, ProviderCatalog>,
    /// Hosts in report order, oldest first, so "the newest catalogue" is one
    /// lookup away. A host re-reporting moves to the back rather than
    /// appearing twice.
    order: Vec<HostId>,
}

impl CatalogRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Records the catalogue a host just reported.
    ///
    /// An empty catalogue still replaces the previous one: "the agent now
    /// advertises no models" is a fact, and keeping stale models alive because
    /// the new answer was empty would offer choices the agent no longer has.
    pub fn record(&self, host_id: &HostId, catalog: ProviderCatalog) {
        let mut inner = self.inner.lock().expect("catalog registry lock");
        if inner.by_host.insert(host_id.clone(), catalog).is_none() {
            inner.order.push(host_id.clone());
            return;
        }
        if let Some(position) = inner.order.iter().position(|known| known == host_id) {
            let host_id = inner.order.remove(position);
            inner.order.push(host_id);
        }
    }

    /// The catalogue one host most recently reported.
    pub fn get(&self, host_id: &HostId) -> Option<ProviderCatalog> {
        self.inner
            .lock()
            .expect("catalog registry lock")
            .by_host
            .get(host_id)
            .cloned()
    }

    /// The newest non-empty catalogue any host reported.
    ///
    /// This is the answer for a caller that named no host — a fresh install's
    /// first composer query carries no environment — so the picker can show
    /// real models before the user has chosen where the thread runs.
    pub fn most_recent(&self) -> Option<ProviderCatalog> {
        let inner = self.inner.lock().expect("catalog registry lock");
        inner
            .order
            .iter()
            .rev()
            .find_map(|host_id| inner.by_host.get(host_id))
            .filter(|catalog| !catalog.is_empty())
            .cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use loom_domain::catalog::{CatalogModel, CatalogThinkingLevel};

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
        registry.record(&first, catalog("a/one"));
        registry.record(&second, catalog("b/two"));
        assert_eq!(
            registry.get(&first).unwrap().current_model.unwrap(),
            "a/one"
        );
        assert_eq!(
            registry.get(&second).unwrap().current_model.unwrap(),
            "b/two"
        );
    }

    /// A caller that named no host sees the newest answer, which is what a
    /// fresh install's first query needs.
    #[test]
    fn the_newest_report_is_the_fallback() {
        let registry = CatalogRegistry::new();
        let first = HostId::mint();
        let second = HostId::mint();
        registry.record(&first, catalog("a/one"));
        registry.record(&second, catalog("b/two"));
        assert_eq!(
            registry.most_recent().unwrap().current_model.unwrap(),
            "b/two"
        );
    }

    /// A host that goes quiet is not the fallback: the newest *host* to report
    /// wins, and only a non-empty answer counts.
    #[test]
    fn an_empty_report_is_not_offered_as_the_fallback() {
        let registry = CatalogRegistry::new();
        let host = HostId::mint();
        registry.record(&host, ProviderCatalog::default());
        assert!(registry.most_recent().is_none());
        assert!(registry.get(&host).unwrap().is_empty());
    }
}
