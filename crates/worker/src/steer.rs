//! Steering a turn that is already running.
//!
//! A **steer** is a user message that belongs to the turn in flight rather than
//! to a new one. ACP has no primitive for injecting input into a running
//! prompt, so the worker delivers it the way every ACP client does — it cancels
//! the prompt in flight and sends the text as the next prompt on the same
//! session, keeping the run open so the terminal event still comes from the
//! last prompt. See [`loom_provider_protocol::RunSteer`] for the wire contract.
//!
//! This module owns the one piece of shared state that makes that possible: a
//! table from a run id to the channel its conversation loop is listening on.
//! The conversation registers itself when its session is established and
//! forgets itself when it ends; a steer naming a run that is not in the table
//! has no live turn to join and is dropped, which is what makes a relay
//! redelivery — and a steer that lost a race with the run's own end —
//! harmless.

use std::collections::HashMap;
use std::sync::Arc;

use loom_domain::RunId;
use tokio::sync::{mpsc, Mutex};

/// How many steers may wait for one run before the sender blocks.
///
/// A steer is a person typing, so this is far more than a real turn can
/// accumulate; the bound exists so a producer cannot grow the queue without
/// limit. The conversation loop drains it as it re-prompts.
const STEER_QUEUE_CAPACITY: usize = 64;

/// The live steer channels, keyed by the run that owns them.
///
/// Cloneable because it is shared between the socket loop (which receives
/// steers) and each run's conversation task (which registers and consumes
/// them).
#[derive(Clone, Default)]
pub struct SteerRegistry {
    senders: Arc<Mutex<HashMap<RunId, mpsc::Sender<String>>>>,
}

impl SteerRegistry {
    /// Creates an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a run's steer channel and returns the receiving half.
    ///
    /// The caller owns the receiver for the life of the run and must call
    /// [`SteerRegistry::forget`] when it ends.
    pub async fn register(&self, run_id: RunId) -> mpsc::Receiver<String> {
        let (sender, receiver) = mpsc::channel(STEER_QUEUE_CAPACITY);
        self.senders.lock().await.insert(run_id, sender);
        receiver
    }

    /// Removes a run's channel.
    ///
    /// Idempotent: forgetting a run that already ended, or that never
    /// registered, is a no-op.
    pub async fn forget(&self, run_id: &RunId) {
        self.senders.lock().await.remove(run_id);
    }

    /// Delivers one steer to a live run.
    ///
    /// Returns `false` when there is no live turn to join, which the caller
    /// reports rather than treating as an error: the run may simply have ended
    /// first.
    pub async fn steer(&self, run_id: &RunId, text: String) -> bool {
        let sender = self.senders.lock().await.get(run_id).cloned();
        match sender {
            Some(sender) => sender.send(text).await.is_ok(),
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_registered_run_receives_its_steer() {
        let registry = SteerRegistry::new();
        let mut receiver = registry.register(RunId::mint()).await;
        let run_id = registry
            .senders
            .lock()
            .await
            .keys()
            .next()
            .cloned()
            .expect("the run registered");
        assert!(registry.steer(&run_id, "look at this".into()).await);
        assert_eq!(receiver.recv().await.as_deref(), Some("look at this"));
    }

    #[tokio::test]
    async fn a_steer_for_an_unknown_run_is_not_delivered() {
        let registry = SteerRegistry::new();
        assert!(!registry.steer(&RunId::mint(), "nobody".into()).await);
    }

    #[tokio::test]
    async fn forgetting_a_run_stops_delivery() {
        let registry = SteerRegistry::new();
        let run_id = RunId::mint();
        let _receiver = registry.register(run_id.clone()).await;
        registry.forget(&run_id).await;
        assert!(!registry.steer(&run_id, "too late".into()).await);
    }
}
