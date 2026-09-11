//! Fixed relay readers.
//!
//! One task per shard, always. This is the point of slicing the log into a
//! constant number of shards: the cost of tailing the log does not grow with
//! the number of conversations, projects or machines. Eight shards means eight
//! tasks and eight cursors, whether the server is idle or hosting thousands of
//! threads.
//!
//! A reader advances a per-shard cursor over [`EventId`]s and hands each record
//! to the hub. Two failure modes are handled explicitly:
//!
//! * **The hub queue is full.** The frame is dropped rather than blocking the
//!   reader. A subscriber that fell behind recovers by replaying from its last
//!   delivered event id, so stalling the reader would trade a recoverable loss
//!   for an unrecoverable one.
//! * **The log was trimmed past the cursor.** The reader resumes from the
//!   oldest surviving record. Trimming only removes records older than the
//!   replay horizon, so this cannot hide anything a reader was entitled to.

use std::time::Duration;

use loom_relay::{Relay, SHARD_COUNT};
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use crate::hub_actor::{DeliveryOutcome, HubHandle};

/// Reader tuning.
#[derive(Clone, Copy, Debug)]
pub struct PumpConfig {
    /// Maximum records a shard reader forwards per pass. Bounds the work one
    /// busy shard can do before yielding.
    pub batch_limit: usize,
    /// How long a reader waits before a periodic catch-up pass when no
    /// notification arrives. Guards against a missed wakeup.
    pub safety_tick: Duration,
}

impl Default for PumpConfig {
    fn default() -> Self {
        Self {
            batch_limit: 256,
            safety_tick: Duration::from_secs(1),
        }
    }
}

/// Runs the fixed set of shard readers.
pub struct Pump {
    notify: std::sync::Arc<Notify>,
    tasks: Vec<JoinHandle<()>>,
    config: PumpConfig,
}

impl Pump {
    /// Starts `SHARD_COUNT` readers over `relay`, delivering into `hub`.
    pub fn spawn(relay: Relay, hub: HubHandle, config: PumpConfig) -> Self {
        let notify = std::sync::Arc::new(Notify::new());
        let mut tasks = Vec::with_capacity(usize::from(SHARD_COUNT));

        for shard in 0..SHARD_COUNT {
            let relay = relay.clone();
            let hub = hub.clone();
            let notify = std::sync::Arc::clone(&notify);
            tasks.push(tokio::spawn(async move {
                read_shard(shard, relay, hub, notify, config).await;
            }));
        }

        Self {
            notify,
            tasks,
            config,
        }
    }

    /// Wakes every reader for an immediate catch-up pass.
    ///
    /// Called after an append so delivery latency is microseconds rather than
    /// one safety tick. `notify_waiters` rather than `notify_one`, because all
    /// shards must be checked — the caller does not know which one changed.
    pub fn wake(&self) {
        self.notify.notify_waiters();
    }

    /// The configuration these readers were started with.
    pub fn config(&self) -> PumpConfig {
        self.config
    }

    /// Number of running readers. Always [`SHARD_COUNT`] until stopped.
    pub fn reader_count(&self) -> usize {
        self.tasks.len()
    }

    /// Stops the readers.
    ///
    /// Aborts rather than drains: readers hold no state that must be flushed,
    /// and a shutdown must not wait on a socket. Callers that own a `Pump`
    /// outright can `await` [`Pump::wait`] for the tasks to finish dying.
    pub fn stop(&self) {
        for task in &self.tasks {
            task.abort();
        }
    }

    /// Waits for every reader task to finish. Only usable by an owner.
    pub async fn wait(self) {
        for task in self.tasks {
            let _ = task.await;
        }
    }
}

impl std::fmt::Debug for Pump {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pump")
            .field("readers", &self.tasks.len())
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

async fn read_shard(
    shard: u8,
    relay: Relay,
    hub: HubHandle,
    notify: std::sync::Arc<Notify>,
    config: PumpConfig,
) {
    let mut cursor: Option<loom_relay::EventId> = None;

    loop {
        // Register interest before reading so an append that lands during the
        // pass still produces a wakeup.
        let waiter = notify.notified();
        tokio::pin!(waiter);

        drain(shard, &relay, &hub, &mut cursor, config.batch_limit);

        tokio::select! {
            () = &mut waiter => {}
            () = tokio::time::sleep(config.safety_tick) => {}
        }
    }
}

/// Reads everything currently available on a shard and forwards it.
///
/// Loops until a pass forwards fewer than `batch_limit` records, so a burst
/// larger than one batch is fully forwarded instead of waiting for a tick.
fn drain(
    shard: u8,
    relay: &Relay,
    hub: &HubHandle,
    cursor: &mut Option<loom_relay::EventId>,
    batch_limit: usize,
) {
    let limit = batch_limit.max(1);
    loop {
        if !hub.is_running() {
            return;
        }

        // The timestamp bound is a safe superset of "after the cursor":
        // event ids are monotonic, so no record after the cursor can carry an
        // earlier timestamp. The id comparison below is the real filter.
        let from_ms = cursor.map(|id| id.timestamp_ms()).unwrap_or(0);
        let Ok(records) = relay.replay_shard(shard, from_ms, limit) else {
            return;
        };

        let mut forwarded = 0usize;
        let mut stopped = false;
        for envelope in records {
            if let Some(current) = *cursor {
                if envelope.event_id <= current {
                    continue;
                }
            }
            *cursor = Some(envelope.event_id);
            match hub.try_deliver(envelope) {
                // A backpressured frame is dropped, but the cursor still
                // advances: the reader must not spin on a subscriber that is
                // behind, and that subscriber has a replay path.
                DeliveryOutcome::Queued | DeliveryOutcome::Backpressured => forwarded += 1,
                DeliveryOutcome::Stopped => {
                    stopped = true;
                    break;
                }
            }
        }

        if stopped || forwarded < limit {
            return;
        }
    }
}
