//! The prompt commands live ACP sessions have advertised, kept per workspace.
//!
//! A command list is the agent's answer for one session: the workspace's prompt
//! files are read from its working directory, and package or skill commands
//! come from the agent's own settings. The key is therefore
//! `(host, provider, cwd)` — the address a `projects.commands` query resolves
//! to — and not the pair the model catalogue uses.
//!
//! The store is deliberately in memory. A list is an advertisement from a live
//! session and the next session in the same workspace reports it again, so a
//! server restart loses nothing durable. The workspace scan remains the
//! fallback for a query no session has answered yet.

use std::collections::HashMap;
use std::sync::Mutex;

use loom_domain::HostId;
use loom_provider_protocol::ProviderCommand;

/// How many workspaces one server remembers before the oldest is dropped.
///
/// Each entry is bounded by the command count the worker advertised, and the
/// address space is workspaces this server has actually run a session in, so
/// the cap exists to bound a long-lived process, not to pace ordinary use.
const MAX_REPORTED_WORKSPACES: usize = 256;

/// Which agent in which workspace a command list belongs to.
type Key = (HostId, String, String);

/// What each session advertised, newest last.
#[derive(Default)]
pub struct CommandRegistry {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    /// The most recent list per `(host, provider, cwd)`.
    by_key: HashMap<Key, Vec<ProviderCommand>>,
    /// Keys in report order, oldest first, so the eviction target is known
    /// without scanning the map. A key re-reporting moves to the back rather
    /// than appearing twice.
    order: Vec<Key>,
}

impl CommandRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Records the list one session advertised.
    ///
    /// An empty list still replaces the previous one: "this session advertised
    /// no commands" is a fact, and keeping stale names alive would offer
    /// commands a restarted agent no longer has.
    pub fn record(
        &self,
        host_id: &HostId,
        provider_id: &str,
        cwd: &str,
        commands: Vec<ProviderCommand>,
    ) {
        let key: Key = (host_id.clone(), provider_id.to_owned(), cwd.to_owned());
        let mut inner = self.inner.lock().expect("command registry lock");
        if inner.by_key.insert(key.clone(), commands).is_none() {
            inner.order.push(key);
        } else if let Some(position) = inner.order.iter().position(|known| known == &key) {
            let key = inner.order.remove(position);
            inner.order.push(key);
        }
        while inner.order.len() > MAX_REPORTED_WORKSPACES {
            let oldest = inner.order.remove(0);
            inner.by_key.remove(&oldest);
        }
    }

    /// What the most recent session in this workspace advertised, if any.
    pub fn get(
        &self,
        host_id: &HostId,
        provider_id: &str,
        cwd: &str,
    ) -> Option<Vec<ProviderCommand>> {
        self.inner
            .lock()
            .expect("command registry lock")
            .by_key
            .get(&(host_id.clone(), provider_id.to_owned(), cwd.to_owned()))
            .cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PI: &str = "pi";

    fn command(name: &str) -> ProviderCommand {
        ProviderCommand {
            name: name.to_owned(),
            description: format!("{name} does things"),
            argument_hint: None,
        }
    }

    /// The working directory is part of the key: the same agent in two
    /// workspaces is two lists, because prompt files are read from disk.
    #[test]
    fn each_workspace_keeps_its_own_list() {
        let registry = CommandRegistry::new();
        let host = HostId::mint();
        registry.record(&host, PI, "/work/one", vec![command("review")]);
        registry.record(&host, PI, "/work/two", vec![command("deploy")]);

        let one = registry.get(&host, PI, "/work/one").unwrap();
        assert_eq!(one[0].name, "review");
        let two = registry.get(&host, PI, "/work/two").unwrap();
        assert_eq!(two[0].name, "deploy");
        assert!(registry.get(&host, PI, "/work/three").is_none());
    }

    /// Two agents on one machine are two lists, not one overwriting the other.
    #[test]
    fn each_provider_on_a_host_keeps_its_own_list() {
        let registry = CommandRegistry::new();
        let host = HostId::mint();
        registry.record(&host, PI, "/work", vec![command("review")]);
        registry.record(&host, "codex", "/work", vec![command("deploy")]);

        assert_eq!(registry.get(&host, PI, "/work").unwrap()[0].name, "review");
        assert_eq!(
            registry.get(&host, "codex", "/work").unwrap()[0].name,
            "deploy"
        );
    }

    /// A newer session replaces the list; an empty one clears it rather than
    /// leaving names the agent no longer advertises.
    #[test]
    fn a_newer_report_replaces_the_list_even_when_empty() {
        let registry = CommandRegistry::new();
        let host = HostId::mint();
        registry.record(&host, PI, "/work", vec![command("review")]);
        registry.record(&host, PI, "/work", Vec::new());

        assert!(registry.get(&host, PI, "/work").unwrap().is_empty());
    }

    /// The registry is bounded: the oldest workspace is dropped rather than
    /// growing without limit over a long-lived process.
    #[test]
    fn the_oldest_workspace_is_evicted_at_the_cap() {
        let registry = CommandRegistry::new();
        let host = HostId::mint();
        for index in 0..=MAX_REPORTED_WORKSPACES {
            registry.record(&host, PI, &format!("/work/{index}"), vec![command("c")]);
        }

        assert!(registry.get(&host, PI, "/work/0").is_none());
        assert!(registry
            .get(&host, PI, &format!("/work/{MAX_REPORTED_WORKSPACES}"))
            .is_some());
    }
}
