//! The entity view, as a durable payload.
//!
//! The relay log is the source of truth for **what happened**. This module holds
//! the **entity view** shape derived from it — projects, threads, hosts,
//! environments, runs, settings and automations — which the store writes whole
//! in one transaction and reads back on the way in.
//!
//! It is deliberately only the shape. The file this used to be framed into,
//! with its magic, CRC and rename-as-commit, is gone: the store's own
//! transaction is the commit, and a database that cannot commit says so instead
//! of leaving a half-written file behind. `docs/domain-persistence.md` has the
//! reasoning for a stored view plus a log delta over the alternatives.
//!
//! A view used to be written every thirty seconds and on shutdown, with the log
//! supplying the delta since its watermark. That is still the shape; what
//! changed is where the baseline lives, and that a run's own record is written
//! as it changes rather than only with the view.

use std::fmt;

use loom_relay::event_id::EventId;
use serde::{Deserialize, Serialize};

use crate::automations::AutomationState;
use crate::domain_state::RegistrySnapshot;
use crate::runs::RunRecord;
use crate::settings::SettingsSnapshot;

/// The payload's shape version. A reader refuses one it does not know.
pub const SNAPSHOT_VERSION: u32 = 1;

/// The entity view plus the log position it was taken at.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DomainSnapshot {
    /// Framing/format version, mirrored from [`SNAPSHOT_VERSION`].
    pub version: u32,
    /// The newest event id the entity view incorporates.
    ///
    /// Recovery replays only events **after** this id. `None` means the view is
    /// empty and the whole retained log should be considered.
    pub watermark: Option<EventId>,
    /// Projects, threads, hosts and environments.
    pub registry: RegistrySnapshot,
    /// Runs that were in flight when the snapshot was taken.
    pub runs: Vec<RunRecord>,
    /// Server-local settings and UI preferences.
    ///
    /// `#[serde(default)]` is the migration path for snapshots written before
    /// B10: those snapshots still restore their domain entities and receive
    /// the current settings defaults.
    #[serde(default)]
    pub settings: Option<SettingsSnapshot>,
    /// Automations and their run history.
    ///
    /// `#[serde(default)]` is the migration path for every snapshot written
    /// before automations existed: the rest of the snapshot restores unchanged
    /// and the workspace simply has no automations. The payload carries its own
    /// version for the same reason settings do — an additive change to one of
    /// them must not force the other to be discarded.
    #[serde(default)]
    pub automations: Option<AutomationState>,
}

/// Why a snapshot could not be read or written.
#[derive(Debug)]
pub enum SnapshotError {
    /// The filesystem refused an operation.
    Io(String),
    /// The file exists but is not a well-formed snapshot.
    Corrupt(String),
    /// The in-memory snapshot could not be encoded.
    Encode(String),
}

impl fmt::Display for SnapshotError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SnapshotError::Io(message) => write!(f, "snapshot io error: {message}"),
            SnapshotError::Corrupt(message) => write!(f, "snapshot is corrupt: {message}"),
            SnapshotError::Encode(message) => write!(f, "snapshot could not be encoded: {message}"),
        }
    }
}

impl std::error::Error for SnapshotError {}
