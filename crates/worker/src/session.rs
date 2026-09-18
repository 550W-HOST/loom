//! The supervised connection loop: connect, run, reconnect, and self-update.
//!
//! [`crate::Worker`] owns one connection. This module owns the *lifecycle*
//! around it, which is where the operational promises live:
//!
//! * a dropped socket is retried with exponential backoff rather than ending
//!   the process, so a server restart, a network blip and a provider crash do
//!   not each need the supervisor;
//! * a server that speaks a newer protocol triggers the update flow
//!   ([`crate::update`]) instead of a hard failure;
//! * a failed update keeps the current worker running and retries on the same
//!   backoff schedule — it never exits into a restart loop, and it never stops
//!   retrying;
//! * a *successful* update ends the session so the supervisor starts the new
//!   binary.
//!
//! The last point is why this returns [`SessionOutcome`] instead of looping
//! forever inside: the caller must be able to exit the process after an
//! install, and systemd's `Restart=always` (or a container's restart policy) is
//! what starts the new file. Nothing replaces a running process in place.

use std::future::Future;
use std::time::Duration;

use loom_domain::HostId;
use loom_relay::EventId;

use crate::update::{UpdateOutcome, Updater};
use crate::{Worker, WorkerConfig};

/// The first delay after a dropped connection.
pub const DEFAULT_RECONNECT_INITIAL: Duration = Duration::from_secs(1);

/// The longest delay between reconnect attempts.
pub const DEFAULT_RECONNECT_MAX: Duration = Duration::from_secs(30);

/// Exponential backoff for reconnects.
///
/// A field rather than a free function because the schedule must survive
/// successes: a worker that connects, drops after a second, and reconnects in a
/// tight loop would otherwise hammer the server with no memory of it. The
/// caller resets it once a connection has been established and has run for a
/// while.
#[derive(Clone, Debug)]
pub struct ReconnectBackoff {
    initial: Duration,
    max: Duration,
    next: Duration,
}

impl ReconnectBackoff {
    /// A schedule starting at `initial`, doubling up to `max`.
    pub fn new(initial: Duration, max: Duration) -> Self {
        let initial = initial.max(Duration::from_millis(1));
        Self {
            initial,
            max: max.max(initial),
            next: initial,
        }
    }

    /// The default schedule: 1 s doubling to 30 s.
    pub fn defaults() -> Self {
        Self::new(DEFAULT_RECONNECT_INITIAL, DEFAULT_RECONNECT_MAX)
    }

    /// Returns the delay to wait before the next attempt and advances.
    pub fn take(&mut self) -> Duration {
        let delay = self.next;
        self.next = (self.next * 2).min(self.max);
        delay
    }

    /// Returns to the first delay, after a connection succeeded.
    pub fn reset(&mut self) {
        self.next = self.initial;
    }

    /// The delay the next [`ReconnectBackoff::take`] will return.
    pub fn peek(&self) -> Duration {
        self.next
    }
}

/// The machine-local state a worker keeps across restarts.
///
/// A trait rather than concrete file paths so the session loop can be tested
/// without a filesystem, and so the one place that knows the on-disk layout
/// stays `main.rs` (next to the host id and cursor it already writes).
pub trait WorkerState: Send + Sync {
    /// The identity to present on connect, if a previous run enrolled one.
    fn host_id(&self) -> Result<Option<HostId>, String>;
    /// Persists a freshly minted identity.
    fn save_host_id(&self, host_id: &HostId) -> Result<(), String>;
    /// The host-scope cursor to replay from.
    fn cursor(&self) -> Result<Option<EventId>, String>;
    /// Persists the advanced cursor.
    fn save_cursor(&self, cursor: Option<&EventId>) -> Result<(), String>;
}

/// Why a supervised session ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionOutcome {
    /// A new worker was installed (or was already installed) and the caller
    /// must exit so the supervisor starts it.
    RestartForUpdate {
        /// What was installed, for the log.
        detail: String,
    },
    /// The caller asked to stop.
    Shutdown,
}

/// Runs a worker until `shutdown` resolves or an update requires a restart.
///
/// `updater` is `None` when self-update is disabled, in which case a protocol
/// mismatch is retried on the reconnect backoff rather than fetched — the
/// worker still never exits into a restart loop.
pub async fn run_session<S, F>(
    config: WorkerConfig,
    updater: Option<&Updater>,
    state: &S,
    shutdown: F,
) -> SessionOutcome
where
    S: WorkerState + ?Sized,
    F: Future<Output = ()>,
{
    // A join code is a one-time enrollment capability. Keep it in the
    // process-local base config only until the first successful enrollment;
    // reconnects use the persisted host id instead.
    let mut config = config;
    // Pinned once here so neither the caller nor the helper needs an `Unpin`
    // bound: an async block that captures a `&mut` is routinely not `Unpin`,
    // and requiring it would be a trap for the one caller that matters.
    let mut shutdown = std::pin::pin!(shutdown);
    let mut reconnect = ReconnectBackoff::defaults();

    loop {
        let mut attempt = config.clone();
        attempt.host_id = match state.host_id() {
            Ok(host_id) => host_id,
            Err(error) => {
                eprintln!("loom-worker: could not read the persisted host id: {error}");
                None
            }
        };
        attempt.resume_cursor = match state.cursor() {
            Ok(cursor) => cursor,
            Err(error) => {
                eprintln!("loom-worker: could not read the persisted cursor: {error}");
                None
            }
        };
        let url = attempt.server_url.clone();

        match Worker::connect(attempt).await {
            Ok(mut worker) => {
                let connected_at = std::time::Instant::now();
                let enrolled = match worker.enroll().await {
                    Ok(enrolled) => enrolled,
                    Err(error) => {
                        eprintln!("loom-worker: enrol failed: {error}");
                        let delay = reconnect.take();
                        if wait_or_shutdown(delay, &mut shutdown).await {
                            return SessionOutcome::Shutdown;
                        }
                        continue;
                    }
                };
                config.join_code = None;
                eprintln!(
                    "loom-worker \"{}\" enrolled as {enrolled} with {url}",
                    config.name
                );
                if let Err(error) = state.save_host_id(&enrolled) {
                    eprintln!("loom-worker: could not persist the host id: {error}");
                }

                let ended = tokio::select! {
                    result = worker.run() => Some(result),
                    _ = shutdown.as_mut() => None,
                };

                let cursor = worker.cursor().cloned();
                if ended.is_none() {
                    eprintln!("loom-worker \"{}\" stopping", config.name);
                    let _ = worker.disconnect().await;
                    if let Err(error) = state.save_cursor(cursor.as_ref()) {
                        eprintln!("loom-worker: could not persist the cursor: {error}");
                    }
                    return SessionOutcome::Shutdown;
                }
                match ended.unwrap() {
                    Ok(()) => {
                        eprintln!("loom-worker \"{}\" lost its server connection", config.name);
                    }
                    Err(error) => {
                        eprintln!("loom-worker \"{}\" connection failed: {error}", config.name);
                    }
                }
                if let Err(error) = state.save_cursor(cursor.as_ref()) {
                    eprintln!("loom-worker: could not persist the cursor: {error}");
                }

                // A connection that stayed up at least as long as the delay the
                // schedule would otherwise impose is treated as healthy, and the
                // schedule starts over. A *flapping* one — a server that accepts
                // `welcome`, lets the host enrol and then drops the socket — does
                // not, so it backs off instead of hot-looping. Resetting
                // unconditionally on connect would make that loop unbounded.
                if connection_was_healthy(connected_at.elapsed(), reconnect.peek()) {
                    reconnect.reset();
                }
                let delay = reconnect.take();
                if wait_or_shutdown(delay, &mut shutdown).await {
                    return SessionOutcome::Shutdown;
                }
            }
            Err(error) => {
                // A version refusal is the one failure with an automatic
                // remedy, and the version is carried in the error so the
                // update is against exactly what the server announced.
                match error.mismatched_protocol_version() {
                    Some(server_protocol_version) => {
                        // Logged before the update runs: the refusal is the
                        // reason the download is happening, and an operator
                        // reading the journal should see both lines.
                        eprintln!("loom-worker \"{}\": {error}", config.name);
                        match updater {
                            Some(updater) => {
                                let outcome = updater.update(server_protocol_version).await;
                                eprintln!(
                                    "loom-worker \"{}\": {}",
                                    config.name,
                                    outcome.describe()
                                );
                                if outcome.should_restart() {
                                    return SessionOutcome::RestartForUpdate {
                                        detail: outcome.describe(),
                                    };
                                }
                                // A failed or backing-off update keeps this
                                // process alive: exiting here would hand the
                                // supervisor a restart loop whose only outcome
                                // is another refused connection.
                                let delay = match outcome {
                                    UpdateOutcome::Failed { retry_in, .. }
                                    | UpdateOutcome::BackingOff { retry_in, .. } => retry_in,
                                    UpdateOutcome::Disabled => reconnect.take(),
                                    // `NotNewer` after a mismatch means the
                                    // server changed under us; retry the
                                    // connection rather than the update.
                                    UpdateOutcome::NotNewer { .. } => reconnect.take(),
                                    _ => reconnect.take(),
                                };
                                if wait_or_shutdown(delay, &mut shutdown).await {
                                    return SessionOutcome::Shutdown;
                                }
                            }
                            None => {
                                let delay = reconnect.take();
                                if wait_or_shutdown(delay, &mut shutdown).await {
                                    return SessionOutcome::Shutdown;
                                }
                            }
                        }
                    }
                    None => {
                        eprintln!("loom-worker \"{}\": {error}", config.name);
                        let delay = reconnect.take();
                        if wait_or_shutdown(delay, &mut shutdown).await {
                            return SessionOutcome::Shutdown;
                        }
                    }
                }
            }
        }
    }
}

/// Whether a connection that lasted `uptime` counts as healthy, given the
/// delay the schedule is currently prepared to impose.
///
/// This is the whole anti-hot-loop rule, in one place: a connection must outlive
/// the backoff it would otherwise have incurred to earn a reset. A server that
/// accepts, enrols and immediately drops therefore sees the delay double until
/// it either behaves or the cap (30 s) applies.
pub fn connection_was_healthy(uptime: Duration, next_delay: Duration) -> bool {
    uptime >= next_delay
}

/// Waits `delay`, or returns early when shutdown arrives. `true` means stop.
async fn wait_or_shutdown<F: Future<Output = ()>>(
    delay: Duration,
    shutdown: &mut std::pin::Pin<&mut F>,
) -> bool {
    // A zero delay must not skip the shutdown check.
    tokio::select! {
        _ = tokio::time::sleep(delay) => false,
        _ = shutdown.as_mut() => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn the_backoff_doubles_from_the_first_delay_and_caps() {
        let mut backoff = ReconnectBackoff::new(Duration::from_secs(1), Duration::from_secs(8));
        assert_eq!(backoff.take(), Duration::from_secs(1));
        assert_eq!(backoff.take(), Duration::from_secs(2));
        assert_eq!(backoff.take(), Duration::from_secs(4));
        assert_eq!(backoff.take(), Duration::from_secs(8));
        assert_eq!(
            backoff.take(),
            Duration::from_secs(8),
            "capped, not growing"
        );
        backoff.reset();
        assert_eq!(backoff.take(), Duration::from_secs(1), "reset restarts it");
    }

    #[test]
    fn a_degenerate_backoff_still_makes_progress() {
        // `max` below `initial` must not produce a zero or shrinking delay.
        let mut backoff = ReconnectBackoff::new(Duration::from_secs(5), Duration::from_millis(1));
        assert_eq!(backoff.take(), Duration::from_secs(5));
        assert_eq!(backoff.take(), Duration::from_secs(5));
        let zero = ReconnectBackoff::new(Duration::ZERO, Duration::ZERO);
        assert!(zero.peek() >= Duration::from_millis(1));
    }

    #[test]
    fn a_flapping_connection_never_resets_the_backoff() {
        // The regression this guards: a server that accepts `welcome`, lets the
        // host enrol, and drops the socket within milliseconds. Resetting on
        // every connect made that a hot loop with no delay at all.
        let mut backoff = ReconnectBackoff::defaults();
        let flap = Duration::from_millis(2);
        let first = backoff.peek();
        let delays: Vec<Duration> = (0..5)
            .map(|_| {
                assert!(
                    !connection_was_healthy(flap, backoff.peek()),
                    "a 2 ms connection must not count as healthy"
                );
                backoff.take()
            })
            .collect();
        assert_eq!(
            delays,
            vec![first, first * 2, first * 4, first * 8, first * 16,],
            "the delay must keep growing across flaps"
        );

        // A connection that outlives the pending delay is healthy and restarts
        // the schedule, which is what keeps a long-lived server hiccup cheap.
        assert!(connection_was_healthy(
            Duration::from_secs(30),
            backoff.peek()
        ));
    }

    /// A state store that records writes in memory, so the session loop can be
    /// exercised without touching a filesystem.
    #[derive(Default)]
    struct MemoryState {
        host_id: Mutex<Option<HostId>>,
        cursor: Mutex<Option<EventId>>,
    }

    impl WorkerState for MemoryState {
        fn host_id(&self) -> Result<Option<HostId>, String> {
            Ok(self.host_id.lock().unwrap().clone())
        }
        fn save_host_id(&self, host_id: &HostId) -> Result<(), String> {
            *self.host_id.lock().unwrap() = Some(host_id.clone());
            Ok(())
        }
        fn cursor(&self) -> Result<Option<EventId>, String> {
            // `EventId` is `Copy`; the lock guard is the only temporary here.
            Ok(*self.cursor.lock().unwrap())
        }
        fn save_cursor(&self, cursor: Option<&EventId>) -> Result<(), String> {
            *self.cursor.lock().unwrap() = cursor.copied();
            Ok(())
        }
    }

    #[tokio::test]
    async fn a_ready_shutdown_ends_the_session_without_waiting_on_the_backoff() {
        let state = MemoryState::default();
        // Port 1 refuses immediately: the loop fails to connect, enters its
        // backoff wait, and the ready shutdown must interrupt that wait rather
        // than the test waiting out the delay.
        let config = WorkerConfig::new("http://127.0.0.1:1", "test-worker");
        let outcome = tokio::time::timeout(
            Duration::from_secs(5),
            run_session(config, None, &state, async {}),
        )
        .await
        .expect("a ready shutdown must not wait on a connection attempt");
        assert_eq!(outcome, SessionOutcome::Shutdown);
    }

    #[tokio::test]
    async fn a_shutdown_during_a_reconnect_wait_ends_the_session() {
        let state = MemoryState::default();
        let config = WorkerConfig::new("http://127.0.0.1:1", "test-worker");
        // The 1 s reconnect delay must be cut short by this 50 ms shutdown.
        let shutdown = async {
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        let started = std::time::Instant::now();
        let outcome = tokio::time::timeout(
            Duration::from_secs(5),
            run_session(config, None, &state, shutdown),
        )
        .await
        .expect("shutdown must interrupt the backoff wait");
        assert_eq!(outcome, SessionOutcome::Shutdown);
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "the backoff wait was not interrupted: {:?}",
            started.elapsed()
        );
    }
}
