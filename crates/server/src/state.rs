//! Shared server state.
//!
//! [`AppState`] is deliberately three things and no more: the log, a handle to
//! the fan-out actor, and the readers connecting them. Handlers get this and
//! nothing else — no database handle, no connection registry. A route can
//! publish an event but cannot decide who receives it, which is the property
//! that keeps routing changes out of handlers.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use loom_relay::retention::Retention;
use loom_relay::{now_ms, Relay, Result as RelayResult};

use crate::hub_actor::HubHandle;
use crate::pump::{Pump, PumpConfig};

/// How the server is wired.
#[derive(Clone, Debug)]
pub struct AppConfig {
    /// This node's identity, stamped on every envelope it produces.
    pub node_id: String,
    /// Approximate records retained per shard.
    pub backend_max_len: usize,
    /// Where the relay log lives on disk.
    ///
    /// `None` keeps the in-process, zero-dependency memory backend, which
    /// is the default so the server starts with no configuration at all.
    /// `Some(path)` uses the durable backend, so the grace window replays
    /// across a restart.
    pub backend_path: Option<PathBuf>,
    /// Retention windows for the log.
    pub retention: Retention,
    /// Bound on the hub actor's command queue.
    pub hub_queue_capacity: usize,
    /// Reader tuning.
    pub pump: PumpConfig,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            node_id: "loom-node".into(),
            backend_max_len: 2_000,
            backend_path: None,
            retention: Retention::default(),
            hub_queue_capacity: 1_024,
            pump: PumpConfig::default(),
        }
    }
}

/// Raised when the server cannot be wired up.
#[derive(Debug)]
pub struct BuildStateError {
    message: String,
}

impl std::fmt::Display for BuildStateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for BuildStateError {}

impl From<loom_relay::RelayError> for BuildStateError {
    fn from(error: loom_relay::RelayError) -> Self {
        Self {
            message: error.to_string(),
        }
    }
}

/// Everything a handler needs.
#[derive(Clone)]
pub struct AppState {
    /// The relay log.
    pub relay: Relay,
    /// Handle to the per-node fan-out actor.
    pub hub: HubHandle,
    /// The fixed shard readers.
    pub pump: Arc<Pump>,
    started_at: Instant,
    started_at_ms: u64,
}

impl AppState {
    /// Wires the relay, hub actor and readers together.
    pub fn build(config: AppConfig) -> Result<Self, BuildStateError> {
        let backend: loom_relay::SharedBackend = match &config.backend_path {
            // Durable backend: replay survives a restart.
            Some(path) => Arc::new(loom_relay::backend::disk::DiskBackend::open(
                path,
                config.backend_max_len,
            )?),
            // Default: in-process, zero external service.
            None => Arc::new(loom_relay::backend::memory::MemoryBackend::new(
                config.backend_max_len,
            )),
        };
        let relay = Relay::new(backend, config.retention, config.node_id.clone())?;
        let (hub, _actor) = HubHandle::spawn(config.hub_queue_capacity);
        let pump = Arc::new(Pump::spawn(relay.clone(), hub.clone(), config.pump));

        Ok(Self {
            relay,
            hub,
            pump,
            started_at: Instant::now(),
            started_at_ms: now_ms(),
        })
    }

    /// Publishes a frame and wakes the readers.
    ///
    /// This is the whole producer API. Note what it does not do: it never
    /// consults a subscriber, a socket or a room.
    ///
    /// The bytes stored in the log are the **client-facing frame**
    /// (`{"type":"event",...}`), built here before the store so a replayed
    /// frame is identical to the live one. See [`crate::protocol`].
    pub fn publish(
        &self,
        scope: loom_relay::Scope,
        payload: impl Into<bytes::Bytes>,
    ) -> RelayResult<loom_relay::Envelope> {
        let payload = payload.into();
        let frame_scope = scope.clone();
        let envelope = self
            .relay
            .publish_with(scope, move |event_id, created_at_ms| {
                crate::protocol::build_event_frame(&frame_scope, &payload, event_id, created_at_ms)
            })?;
        self.pump.wake();
        Ok(envelope)
    }

    /// Milliseconds since the server was wired up.
    pub fn uptime_ms(&self) -> u64 {
        self.started_at.elapsed().as_millis() as u64
    }

    /// Wall-clock milliseconds at startup.
    pub fn started_at_ms(&self) -> u64 {
        self.started_at_ms
    }

    /// Stops the readers. `&self` because the pump is shared via `Arc`.
    pub fn shutdown(&self) {
        self.pump.stop();
    }
}

impl std::fmt::Debug for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppState")
            .field("relay", &self.relay)
            .field("readers", &self.pump.reader_count())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use loom_relay::Scope;
    use tempfile::TempDir;

    #[tokio::test]
    async fn publish_reaches_a_subscriber_end_to_end() {
        use loom_relay_hub::RecordingTransport;

        let state = AppState::build(AppConfig::default()).unwrap();
        let transport = RecordingTransport::new();
        let scope = Scope::Thread("thr_1".into());

        state
            .hub
            .connect(Box::new(transport.clone()), scope.clone())
            .await
            .unwrap();
        let envelope = state.publish(scope, "{\"n\":1}").unwrap();

        // Delivery is asynchronous: producer -> log -> reader -> actor -> sink.
        for _ in 0..200 {
            if !transport.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let frames = transport.frames();
        assert_eq!(frames.len(), 1);
        let frame: serde_json::Value = serde_json::from_str(&frames[0]).unwrap();
        assert_eq!(frame["type"], "event");
        assert_eq!(frame["payload"], "{\"n\":1}");
        assert_eq!(frame["event_id"], envelope.event_id.to_string());

        state.shutdown();
    }

    #[tokio::test]
    async fn a_published_event_is_replayable() {
        let state = AppState::build(AppConfig::default()).unwrap();
        let scope = Scope::Thread("thr_1".into());
        let envelope = state.publish(scope.clone(), "{\"n\":1}").unwrap();

        let replayed = state.relay.replay_scope(&scope, 10).unwrap();
        assert_eq!(replayed.len(), 1);
        assert_eq!(replayed[0].event_id, envelope.event_id);

        state.shutdown();
    }

    #[tokio::test]
    async fn a_configured_data_directory_switches_to_the_durable_backend() {
        let dir = TempDir::new().unwrap();
        let config = AppConfig {
            backend_path: Some(dir.path().to_path_buf()),
            ..AppConfig::default()
        };
        let state = AppState::build(config).unwrap();
        let scope = Scope::Thread("thr_1".into());
        let envelope = state.publish(scope.clone(), "{\"n\":1}").unwrap();

        // The default backend stays in-process; with a data directory the log
        // is materialised on disk as well.
        assert!(dir
            .path()
            .join(format!("shard-{}.log", scope.shard()))
            .exists());

        let replayed = state.relay.replay_scope(&scope, 10).unwrap();
        assert_eq!(replayed.len(), 1);
        assert_eq!(replayed[0].event_id, envelope.event_id);

        state.shutdown();
    }

    #[tokio::test]
    async fn reports_a_plausible_uptime() {
        let state = AppState::build(AppConfig::default()).unwrap();
        assert!(state.uptime_ms() < 10_000);
        assert!(state.started_at_ms() > 0);
    }

    #[tokio::test]
    async fn the_reader_count_is_fixed_by_shard_count() {
        let state = AppState::build(AppConfig::default()).unwrap();
        assert_eq!(
            state.pump.reader_count(),
            usize::from(loom_relay::SHARD_COUNT)
        );
    }
}
