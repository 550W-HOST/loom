//! Daemon self-update: what a daemon does when the server speaks a newer
//! protocol.
//!
//! A server and a daemon must agree on [`loom_server::PROTOCOL_VERSION`] to
//! exchange a single frame, and upgrading every execution machine by hand is the
//! operational trap this module removes. The design is:
//!
//! 1. **Pull, not push.** The daemon asks on connect; the server never tells a
//!    connected daemon to update. Everything needed is already in the handshake
//!    — the server's protocol version arrives in the internal `hello` frame — so the
//!    daemon needs no update state machine driven by the control plane and the
//!    server never has to track who is current. See `docs/upgrades.md` for the
//!    conclusion and the trade-offs against a push design.
//! 2. **The server hosts the artifact that matches it.** [`ArtifactClient`]
//!    fetches `/install/loom-daemon` from the same server whose protocol did not
//!    match, so "the binary is compatible with this server" is true by
//!    construction. No release lookup, no version matrix, no external service.
//! 3. **Verify before install, install atomically, keep the old binary until the
//!    last moment.** The download's SHA-256 is checked against the server's own
//!    digest, and the new file is written beside the running binary and
//!    `rename`d over it. A failed download, a failed digest and a failed write
//!    all leave the current process running the current binary.
//! 4. **Exit for the supervisor; never replace the running process.** After a
//!    successful install the daemon exits and systemd's `Restart=always` starts
//!    the new binary. loom already makes a restart safe — the host identity and
//!    the replay cursor are persisted — so the restart is the update, not a side
//!    effect of one.
//! 5. **Back off, do not spin.** Attempts are counted per target protocol
//!    version and persisted, so a daemon whose new binary still mismatches
//!    retries on an exponential schedule (5 s doubling to 5 min) instead of
//!    restart-looping.
//!
//! # What happens to a run that is in flight
//!
//! An update is only attempted on a connection that has not enrolled — the
//! handshake is refused before any dispatch is read — so the process doing the
//! update has no run in flight on that connection. A run that was in flight on
//! the *previous* connection is a different case and is unchanged from any other
//! disconnect: the provider process is gone with the old process, the server's
//! reaper terminates the run (`host_stale`, or `timed_out` past its deadline),
//! and the thread leaves `working`. It is not resumed, because the dispatch was
//! published before the daemon's persisted cursor and is therefore not replayed;
//! re-issue the turn. See `docs/upgrades.md` § In-flight runs.

use std::path::{Path, PathBuf};
use std::time::Duration;

use loom_server::artifacts::{sha256_hex, valid_sha256_hex, ArtifactClient, ArtifactDownload};
use loom_server::PROTOCOL_VERSION;
use serde::{Deserialize, Serialize};

/// How long the first retry waits, matching bb's daemon self-update.
pub const DEFAULT_INITIAL_BACKOFF: Duration = Duration::from_secs(5);

/// The longest a retry waits, matching bb's daemon self-update.
pub const DEFAULT_MAX_BACKOFF: Duration = Duration::from_secs(5 * 60);

/// File the attempt counter lives in, inside the daemon's state directory.
pub const ATTEMPT_FILE: &str = "host-daemon-update-attempt.json";

/// File the digest of the last successfully installed artifact lives in.
pub const INSTALLED_DIGEST_FILE: &str = "host-artifact.sha256";

/// How the daemon should behave when the server's protocol does not match.
#[derive(Clone, Debug)]
pub struct UpdateConfig {
    /// Whether self-update is permitted. An operator turns it off with
    /// `--no-auto-update` / `LOOM_AUTO_UPDATE=0`; the reason is logged and the
    /// daemon then refuses a mismatched server loudly instead of fetching.
    pub enabled: bool,
    /// The binary to replace. Defaults to the running executable.
    pub install_path: PathBuf,
    /// Where the attempt counter and installed digest are kept. `None` keeps
    /// them in memory only, which is what a daemon started without `--state`
    /// gets: it still updates, it just cannot back off across restarts.
    pub state_dir: Option<PathBuf>,
    /// The target triple to fetch: the one this binary was built for.
    pub target: String,
    /// First retry delay.
    pub initial_backoff: Duration,
    /// Cap on the retry delay.
    pub max_backoff: Duration,
}

impl UpdateConfig {
    /// A configuration for the running executable, on this build's target.
    pub fn for_current_binary(enabled: bool, state_dir: Option<PathBuf>) -> Result<Self, String> {
        let install_path = std::env::current_exe()
            .map_err(|error| format!("could not resolve the running executable: {error}"))?;
        Ok(Self {
            enabled,
            install_path,
            state_dir,
            target: loom_server::TARGET.to_owned(),
            initial_backoff: DEFAULT_INITIAL_BACKOFF,
            max_backoff: DEFAULT_MAX_BACKOFF,
        })
    }
}

/// A persisted, per-protocol-version update attempt.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateAttempt {
    /// When the last attempt started, wall-clock milliseconds.
    pub attempted_at_ms: u64,
    /// How many attempts have been made for this protocol version.
    pub attempt_count: u32,
    /// The server protocol version the attempts are against.
    pub protocol_version: u32,
}

/// What the daemon should do after asking the server whether it is current.
///
/// Every variant is a decision, not an error: a daemon that cannot update keeps
/// running its current binary and retries on the backoff schedule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UpdateOutcome {
    /// Self-update was disabled by the operator.
    Disabled,
    /// The server does not speak a newer protocol. Nothing to do — and a
    /// **downgrade is never attempted**, which is what keeps a daemon that is
    /// ahead of an old server from regressing itself.
    NotNewer {
        /// The server's protocol version.
        server_protocol_version: u32,
        /// Why no update is being attempted.
        reason: String,
    },
    /// The server's artifact is the digest already installed. Exit so the
    /// supervisor starts it.
    AlreadyCurrent {
        /// The agreed-upon target protocol version.
        protocol_version: u32,
        /// The server's crate version.
        version: String,
        /// The installed artifact's digest.
        digest: String,
    },
    /// Downloaded, verified and installed. The caller should exit, leaving the
    /// supervisor to start the new binary.
    Installed {
        /// The agreed-upon target protocol version.
        protocol_version: u32,
        /// The server's crate version.
        version: String,
        /// The installed artifact's digest.
        digest: String,
        /// Where it was installed.
        path: PathBuf,
    },
    /// The attempt failed. The current binary is untouched and still running;
    /// retry after `retry_in`.
    Failed {
        /// What went wrong, for the log.
        reason: String,
        /// How long until the next attempt.
        retry_in: Duration,
    },
    /// The backoff window from a previous attempt has not elapsed.
    BackingOff {
        /// How long until the next attempt.
        retry_in: Duration,
        /// Attempts already made for this protocol version.
        attempt_count: u32,
    },
}

impl UpdateOutcome {
    /// Whether this outcome means "exit and let the supervisor start the new
    /// binary".
    pub fn should_restart(&self) -> bool {
        matches!(
            self,
            UpdateOutcome::Installed { .. } | UpdateOutcome::AlreadyCurrent { .. }
        )
    }

    /// A one-line description for the daemon's log.
    pub fn describe(&self) -> String {
        match self {
            UpdateOutcome::Disabled => "self-update disabled by configuration".into(),
            UpdateOutcome::NotNewer { reason, .. } => reason.clone(),
            UpdateOutcome::AlreadyCurrent {
                version, digest, ..
            } => format!(
                "the server's daemon artifact {version} ({digest}) is already installed; \
                 restarting to run it"
            ),
            UpdateOutcome::Installed {
                version,
                digest,
                path,
                ..
            } => format!(
                "installed daemon {version} ({digest}) at {}; restarting to run it",
                path.display()
            ),
            UpdateOutcome::Failed { reason, retry_in } => format!(
                "self-update failed: {reason}; keeping the current daemon and retrying in {:.1}s",
                retry_in.as_secs_f64()
            ),
            UpdateOutcome::BackingOff {
                retry_in,
                attempt_count,
            } => format!(
                "self-update is backing off after {attempt_count} attempt(s); retrying in {:.1}s",
                retry_in.as_secs_f64()
            ),
        }
    }
}

/// Delays for successive attempts: `initial * 2^(count-1)`, capped.
///
/// The exponent is bounded before shifting so a corrupted or hand-edited
/// attempt file cannot overflow the multiply.
pub fn retry_delay(attempt_count: u32, initial: Duration, max: Duration) -> Duration {
    let exponent = attempt_count.saturating_sub(1).min(20);
    let scaled = initial.saturating_mul(1u32 << exponent);
    scaled.min(max)
}

/// Whether an attempt is due, given the previous one for the same protocol.
///
/// `None` means no previous attempt, which is always due. The result is the
/// delay still to wait: zero when due.
pub fn backoff_remaining_ms(
    now_ms: u64,
    previous: Option<&UpdateAttempt>,
    initial: Duration,
    max: Duration,
) -> u64 {
    let Some(previous) = previous else { return 0 };
    let delay = retry_delay(previous.attempt_count.max(1), initial, max)
        .as_millis()
        .min(u128::from(u64::MAX)) as u64;
    let due_at = previous.attempted_at_ms.saturating_add(delay);
    due_at.saturating_sub(now_ms)
}

/// Performs the update. One instance lasts the process.
pub struct Updater {
    config: UpdateConfig,
    client: ArtifactClient,
    now_ms: Box<dyn Fn() -> u64 + Send + Sync>,
}

impl std::fmt::Debug for Updater {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Updater")
            .field("enabled", &self.config.enabled)
            .field("install_path", &self.config.install_path)
            .field("target", &self.config.target)
            .finish()
    }
}

impl Updater {
    /// Builds an updater for `server_url` (the same URL the daemon dials).
    pub fn new(config: UpdateConfig, server_url: &str) -> Result<Self, String> {
        let client = ArtifactClient::new(server_url)?;
        Ok(Self {
            config,
            client,
            now_ms: Box::new(loom_relay::now_ms),
        })
    }

    /// Replaces the clock. Tests use this to drive the backoff schedule without
    /// sleeping.
    pub fn with_clock(mut self, now_ms: impl Fn() -> u64 + Send + Sync + 'static) -> Self {
        self.now_ms = Box::new(now_ms);
        self
    }

    /// The configuration, for the startup log.
    pub fn config(&self) -> &UpdateConfig {
        &self.config
    }

    /// Runs the update flow for a server that announced `server_protocol_version`.
    ///
    /// The backoff gate is honoured; [`Updater::update_force`] bypasses it for a
    /// test or an explicit operator retry.
    pub async fn update(&self, server_protocol_version: u32) -> UpdateOutcome {
        if !self.config.enabled {
            return UpdateOutcome::Disabled;
        }
        if server_protocol_version <= PROTOCOL_VERSION {
            return UpdateOutcome::NotNewer {
                server_protocol_version,
                reason: if server_protocol_version < PROTOCOL_VERSION {
                    format!(
                        "server speaks protocol {server_protocol_version}, this daemon speaks \
                         {PROTOCOL_VERSION}; refusing to downgrade"
                    )
                } else {
                    format!(
                        "server speaks protocol {server_protocol_version}, matching this daemon; \
                         no update needed"
                    )
                },
            };
        }

        let now = (self.now_ms)();
        let previous = self
            .read_attempt()
            .filter(|attempt| attempt.protocol_version == server_protocol_version);
        let remaining = backoff_remaining_ms(
            now,
            previous.as_ref(),
            self.config.initial_backoff,
            self.config.max_backoff,
        );
        if remaining > 0 {
            return UpdateOutcome::BackingOff {
                retry_in: Duration::from_millis(remaining),
                attempt_count: previous.map(|attempt| attempt.attempt_count).unwrap_or(0),
            };
        }

        let attempt = UpdateAttempt {
            attempted_at_ms: now,
            attempt_count: previous
                .as_ref()
                .map_or(1, |a| a.attempt_count.saturating_add(1)),
            protocol_version: server_protocol_version,
        };
        if let Err(error) = self.write_attempt(&attempt) {
            // A state directory that cannot be written is not fatal: the update
            // can still proceed, it just cannot back off across restarts. Say
            // so rather than refusing to update.
            eprintln!("loom-daemon: could not record the update attempt: {error}");
        }
        self.attempt_with_backoff(attempt.attempt_count).await
    }

    /// The same, ignoring the backoff gate and starting a fresh attempt count.
    pub async fn update_force(&self, server_protocol_version: u32) -> UpdateOutcome {
        if !self.config.enabled {
            return UpdateOutcome::Disabled;
        }
        if server_protocol_version <= PROTOCOL_VERSION {
            return UpdateOutcome::NotNewer {
                server_protocol_version,
                reason: format!(
                    "server speaks protocol {server_protocol_version}, this daemon speaks \
                     {PROTOCOL_VERSION}; refusing to update"
                ),
            };
        }
        let now = (self.now_ms)();
        let attempt = UpdateAttempt {
            attempted_at_ms: now,
            attempt_count: 1,
            protocol_version: server_protocol_version,
        };
        if let Err(error) = self.write_attempt(&attempt) {
            eprintln!("loom-daemon: could not record the update attempt: {error}");
        }
        self.attempt_with_backoff(attempt.attempt_count).await
    }

    /// The body of an attempt: fetch, verify, install.
    async fn attempt_with_backoff(&self, attempt_count: u32) -> UpdateOutcome {
        let retry_in = retry_delay(
            attempt_count.saturating_add(1),
            self.config.initial_backoff,
            self.config.max_backoff,
        );
        match self.attempt().await {
            Ok(outcome) => outcome,
            Err(reason) => UpdateOutcome::Failed { reason, retry_in },
        }
    }

    async fn attempt(&self) -> Result<UpdateOutcome, String> {
        let server = self.client.install_version().await?;
        // The welcome frame said one protocol version and the install endpoint
        // says another: a load-balanced or mid-upgrade fleet. Do not install a
        // binary for a version that is no longer current; retry.
        if server.protocol_version <= PROTOCOL_VERSION {
            return Ok(UpdateOutcome::NotNewer {
                server_protocol_version: server.protocol_version,
                reason: format!(
                    "the server now reports protocol {}; no update needed",
                    server.protocol_version
                ),
            });
        }

        let installed_digest = self.read_installed_digest();
        let download = self
            .client
            .artifact(&self.config.target, installed_digest.as_deref())
            .await?;

        let (digest, bytes) = match download {
            ArtifactDownload::NotModified => {
                let digest = installed_digest.ok_or_else(|| {
                    "the server answered 304 but no installed digest was recorded".to_owned()
                })?;
                return Ok(UpdateOutcome::AlreadyCurrent {
                    protocol_version: server.protocol_version,
                    version: server.version,
                    digest,
                });
            }
            ArtifactDownload::Artifact { digest, bytes } => (digest, bytes),
        };

        // The digest the server *sent* is the authority; recompute it from the
        // bytes actually received. A truncated download fails here, before
        // anything is installed.
        if !valid_sha256_hex(&digest) {
            return Err(format!("the server sent a malformed digest {digest:?}"));
        }
        let actual = sha256_hex(&bytes);
        if actual != digest {
            return Err(format!(
                "artifact digest mismatch: the server said {digest}, the download is {actual}"
            ));
        }

        let path = self.install(&bytes).await?;
        if let Err(error) = self.write_installed_digest(&digest) {
            eprintln!("loom-daemon: could not record the installed artifact digest: {error}");
        }
        Ok(UpdateOutcome::Installed {
            protocol_version: server.protocol_version,
            version: server.version,
            digest,
            path,
        })
    }

    /// Writes the artifact beside the running binary and renames it into place.
    ///
    /// Write-then-rename, both in the install directory, is what makes this
    /// atomic: the running process keeps its image (an open file is not
    /// disturbed by a rename over its name), and a daemon that restarts
    /// mid-write finds either the old binary or the complete new one. The
    /// temporary file is removed on every failure path.
    async fn install(&self, bytes: &[u8]) -> Result<PathBuf, String> {
        let path = self.config.install_path.clone();
        let dir = path
            .parent()
            .ok_or_else(|| format!("{} has no parent directory", path.display()))?
            .to_path_buf();
        let temporary = dir.join(format!(
            ".{}.update.{}",
            path.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("loom-daemon"),
            std::process::id()
        ));

        let result = write_executable(&temporary, bytes).await;
        if let Err(error) = result {
            let _ = tokio::fs::remove_file(&temporary).await;
            return Err(error);
        }
        if let Err(error) = tokio::fs::rename(&temporary, &path).await {
            let _ = tokio::fs::remove_file(&temporary).await;
            return Err(format!("could not replace {}: {error}", path.display()));
        }
        Ok(path)
    }

    fn attempt_path(&self) -> Option<PathBuf> {
        self.config
            .state_dir
            .as_ref()
            .map(|dir| dir.join(ATTEMPT_FILE))
    }

    fn installed_digest_path(&self) -> Option<PathBuf> {
        self.config
            .state_dir
            .as_ref()
            .map(|dir| dir.join(INSTALLED_DIGEST_FILE))
    }

    fn read_attempt(&self) -> Option<UpdateAttempt> {
        let path = self.attempt_path()?;
        let raw = std::fs::read_to_string(path).ok()?;
        let attempt: UpdateAttempt = serde_json::from_str(raw.trim()).ok()?;
        (attempt.attempt_count > 0 && attempt.protocol_version > 0).then_some(attempt)
    }

    fn write_attempt(&self, attempt: &UpdateAttempt) -> Result<(), String> {
        let Some(path) = self.attempt_path() else {
            return Ok(());
        };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|error| error.to_string())?;
        }
        let encoded = serde_json::to_string(attempt).map_err(|error| error.to_string())?;
        atomic_write(&path, encoded.as_bytes(), 0o600)
    }

    fn read_installed_digest(&self) -> Option<String> {
        let path = self.installed_digest_path()?;
        let digest = std::fs::read_to_string(path).ok()?;
        let digest = digest.trim().to_owned();
        valid_sha256_hex(&digest).then_some(digest)
    }

    fn write_installed_digest(&self, digest: &str) -> Result<(), String> {
        let Some(path) = self.installed_digest_path() else {
            return Ok(());
        };
        atomic_write(&path, format!("{digest}\n").as_bytes(), 0o600)
    }
}

/// Writes a file with a temporary sibling and a rename, so a crash cannot leave
/// a half-written one.
fn atomic_write(path: &Path, bytes: &[u8], mode: u32) -> Result<(), String> {
    let temporary = path.with_extension(format!("{}.tmp", std::process::id()));
    write_with_mode(&temporary, bytes, mode)
        .map_err(|error| format!("{}: {error}", temporary.display()))?;
    std::fs::rename(&temporary, path).map_err(|error| {
        let _ = std::fs::remove_file(&temporary);
        format!("{}: {error}", path.display())
    })
}

fn write_with_mode(path: &Path, bytes: &[u8], mode: u32) -> std::io::Result<()> {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)?;
    file.write_all(bytes)?;
    // The mode is set before the file becomes visible at its final name.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    let _ = mode;
    file.sync_all()
}

async fn write_executable(path: &Path, bytes: &[u8]) -> Result<(), String> {
    tokio::fs::write(path, bytes)
        .await
        .map_err(|error| format!("{}: {error}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
            .await
            .map_err(|error| format!("{}: {error}", path.display()))?;
    }
    // Flush before the rename: a rename that reaches disk before the data would
    // leave a new name pointing at an incomplete file after a power loss.
    let file = tokio::fs::File::open(path)
        .await
        .map_err(|error| format!("{}: {error}", path.display()))?;
    file.sync_all()
        .await
        .map_err(|error| format!("{}: {error}", path.display()))?;
    drop(file);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_downgrade_is_never_attempted() {
        let config = UpdateConfig {
            enabled: true,
            install_path: PathBuf::from("/nonexistent/loom-daemon"),
            state_dir: None,
            target: "x86_64-unknown-linux-musl".into(),
            initial_backoff: DEFAULT_INITIAL_BACKOFF,
            max_backoff: DEFAULT_MAX_BACKOFF,
        };
        let updater = Updater::new(config, "http://127.0.0.1:1").unwrap();
        // An older server must not reach the network at all.
        let outcome = updater.update(PROTOCOL_VERSION - 1).await;
        match outcome {
            UpdateOutcome::NotNewer {
                server_protocol_version,
                reason,
            } => {
                assert_eq!(server_protocol_version, PROTOCOL_VERSION - 1);
                assert!(reason.contains("downgrade"), "{reason}");
            }
            other => panic!("expected NotNewer, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_matching_server_needs_no_update() {
        let config = UpdateConfig {
            enabled: true,
            install_path: PathBuf::from("/nonexistent/loom-daemon"),
            state_dir: None,
            target: "t".into(),
            initial_backoff: DEFAULT_INITIAL_BACKOFF,
            max_backoff: DEFAULT_MAX_BACKOFF,
        };
        let updater = Updater::new(config, "http://127.0.0.1:1").unwrap();
        let outcome = updater.update(PROTOCOL_VERSION).await;
        assert!(matches!(outcome, UpdateOutcome::NotNewer { .. }));
    }

    #[tokio::test]
    async fn a_disabled_updater_does_nothing() {
        let config = UpdateConfig {
            enabled: false,
            install_path: PathBuf::from("/nonexistent/loom-daemon"),
            state_dir: None,
            target: "t".into(),
            initial_backoff: DEFAULT_INITIAL_BACKOFF,
            max_backoff: DEFAULT_MAX_BACKOFF,
        };
        let updater = Updater::new(config, "http://127.0.0.1:1").unwrap();
        let outcome = updater.update(PROTOCOL_VERSION + 5).await;
        assert_eq!(outcome, UpdateOutcome::Disabled);
    }

    #[test]
    fn the_retry_delay_doubles_and_caps() {
        let initial = Duration::from_secs(5);
        let max = Duration::from_secs(300);
        assert_eq!(retry_delay(1, initial, max), Duration::from_secs(5));
        assert_eq!(retry_delay(2, initial, max), Duration::from_secs(10));
        assert_eq!(retry_delay(3, initial, max), Duration::from_secs(20));
        assert_eq!(retry_delay(7, initial, max), Duration::from_secs(300));
        // A hand-edited attempt count cannot overflow.
        assert_eq!(retry_delay(u32::MAX, initial, max), max);
        assert_eq!(retry_delay(0, initial, max), Duration::from_secs(5));
    }

    #[test]
    fn the_backoff_gate_opens_when_the_delay_elapses() {
        let initial = Duration::from_secs(5);
        let max = Duration::from_secs(300);
        assert_eq!(backoff_remaining_ms(1_000, None, initial, max), 0);

        let previous = UpdateAttempt {
            attempted_at_ms: 10_000,
            attempt_count: 2,
            protocol_version: 2,
        };
        // 10 s delay after the second attempt.
        assert_eq!(
            backoff_remaining_ms(10_000, Some(&previous), initial, max),
            10_000
        );
        assert_eq!(
            backoff_remaining_ms(15_000, Some(&previous), initial, max),
            5_000
        );
        assert_eq!(
            backoff_remaining_ms(20_000, Some(&previous), initial, max),
            0
        );
        assert_eq!(
            backoff_remaining_ms(99_000, Some(&previous), initial, max),
            0
        );
    }

    #[test]
    fn an_attempt_round_trips_through_the_state_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut config =
            UpdateConfig::for_current_binary(true, Some(dir.path().to_path_buf())).unwrap();
        config.target = "x86_64-unknown-linux-musl".into();
        let updater = Updater::new(config, "http://127.0.0.1:1").unwrap();
        assert_eq!(updater.read_attempt(), None);

        let attempt = UpdateAttempt {
            attempted_at_ms: 42,
            attempt_count: 3,
            protocol_version: 7,
        };
        updater.write_attempt(&attempt).unwrap();
        assert_eq!(updater.read_attempt(), Some(attempt.clone()));

        // A corrupted file reads as "no attempt" rather than failing the update.
        std::fs::write(dir.path().join(ATTEMPT_FILE), b"{ this is not json").unwrap();
        assert_eq!(updater.read_attempt(), None);
    }

    #[test]
    fn an_installed_digest_is_only_read_back_when_it_is_one() {
        let dir = tempfile::tempdir().unwrap();
        let config =
            UpdateConfig::for_current_binary(true, Some(dir.path().to_path_buf())).unwrap();
        let updater = Updater::new(config, "http://127.0.0.1:1").unwrap();
        assert_eq!(updater.read_installed_digest(), None);

        let digest = sha256_hex(b"installed");
        updater.write_installed_digest(&digest).unwrap();
        assert_eq!(updater.read_installed_digest(), Some(digest));

        std::fs::write(dir.path().join(INSTALLED_DIGEST_FILE), "not-a-digest\n").unwrap();
        assert_eq!(updater.read_installed_digest(), None);
    }

    #[tokio::test]
    async fn installing_replaces_the_file_atomically_and_leaves_no_temporary() {
        let dir = tempfile::tempdir().unwrap();
        let install_path = dir.path().join("loom-daemon");
        std::fs::write(&install_path, b"old binary").unwrap();

        let mut config = UpdateConfig::for_current_binary(true, None).unwrap();
        config.install_path = install_path.clone();
        let updater = Updater::new(config, "http://127.0.0.1:1").unwrap();

        let path = updater.install(b"new binary").await.unwrap();
        assert_eq!(path, install_path);
        assert_eq!(std::fs::read(&install_path).unwrap(), b"new binary");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&install_path)
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o755, "the replacement must be executable");
        }
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains("update"))
            .collect();
        assert!(leftovers.is_empty(), "temporary files left: {leftovers:?}");
    }

    #[tokio::test]
    async fn a_failed_install_leaves_the_current_binary_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let install_path = dir.path().join("loom-daemon");
        std::fs::write(&install_path, b"old binary").unwrap();

        let mut config = UpdateConfig::for_current_binary(true, None).unwrap();
        // A directory where the binary should be: the rename must fail.
        config.install_path = dir.path().join("a-directory");
        std::fs::create_dir(config.install_path.clone()).unwrap();
        let updater = Updater::new(config, "http://127.0.0.1:1").unwrap();

        let error = updater.install(b"new binary").await.unwrap_err();
        assert!(error.contains("a-directory"), "{error}");
        assert_eq!(
            std::fs::read(&install_path).unwrap(),
            b"old binary",
            "the old binary must survive a failed install"
        );
    }
}
