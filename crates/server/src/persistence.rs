//! Crash-safe domain-state snapshots.
//!
//! The relay log is the source of truth for **what happened**. This module
//! stores the **entity view** derived from it — projects, threads, hosts and
//! environments — so that a restart can resume without replaying a timeline
//! that in part predates the retention window.
//!
//! # Why a snapshot at all
//!
//! Replaying the log rebuilds the entity view only for entities whose creation
//! event is still retained. Retention trims oldest-first, so an old project or
//! thread loses its `*_created` event long before it stops mattering. Pure
//! replay (`option b`) therefore cannot be the whole answer. A snapshot is the
//! baseline; the retained log supplies the delta since that baseline. See
//! `docs/domain-persistence.md` for the full trade-off.
//!
//! # Crash safety
//!
//! A snapshot is written to a temporary file, `fsync`ed, then `rename`d over
//! the live path, and the directory is `fsync`ed. `rename` is atomic on every
//! filesystem this project targets, so a reader sees either the complete
//! previous snapshot or the complete new one — never a half-written file. The
//! framed envelope (magic, length and CRC-32) is a second line of defence:
//! a truncated or bit-rotted file is detected and reported instead of being
//! deserialized into a plausible-looking but wrong domain state.
//!
//! The worst case is therefore a roll-back to the previous consistent point,
//! never corruption. The log delta since that point is replayed on top.

use std::fmt;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

use loom_relay::event_id::EventId;
use serde::{Deserialize, Serialize};

use crate::automations::AutomationState;
use crate::domain_state::RegistrySnapshot;
use crate::runs::RunRecord;
use crate::settings::SettingsSnapshot;

/// Name of the snapshot file inside the data directory.
pub const SNAPSHOT_FILE: &str = "domain.snapshot";
/// Suffix of the temporary file the snapshot is staged in.
const TEMP_SUFFIX: &str = "tmp";
/// Marks the start of a framed snapshot.
const SNAPSHOT_MAGIC: &[u8; 8] = b"LOOMSNAP";
/// Framing version. Bumping it makes older files unreadable rather than wrong.
pub const SNAPSHOT_VERSION: u32 = 1;
/// Fixed header: magic, version, payload length, payload CRC.
const HEADER_LEN: usize = 8 + 4 + 8 + 4;
/// Upper bound on a snapshot body, so garbage lengths fail fast.
const MAX_SNAPSHOT_LEN: u64 = 256 * 1024 * 1024;

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

/// The live snapshot path under a data directory.
pub fn snapshot_path(root: &Path) -> PathBuf {
    root.join(SNAPSHOT_FILE)
}

/// Reads the snapshot under `root`, if there is one.
///
/// `Ok(None)` means "nothing to restore": no file yet. A file that exists but
/// cannot be trusted is an `Err`, so the caller can decide to fall back to the
/// log rather than silently start from an empty view.
pub fn read_snapshot(root: &Path) -> Result<Option<DomainSnapshot>, SnapshotError> {
    let path = snapshot_path(root);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(SnapshotError::Io(error.to_string())),
    };
    decode(&bytes).map(Some)
}

/// Writes a snapshot atomically under `root`.
pub fn write_snapshot(root: &Path, snapshot: &DomainSnapshot) -> Result<(), SnapshotError> {
    fs::create_dir_all(root).map_err(|error| SnapshotError::Io(error.to_string()))?;
    let payload =
        serde_json::to_vec(snapshot).map_err(|error| SnapshotError::Encode(error.to_string()))?;
    let framed = encode(&payload);

    let temp = root.join(format!("{SNAPSHOT_FILE}.{TEMP_SUFFIX}"));
    {
        let mut file = File::create(&temp).map_err(|error| SnapshotError::Io(error.to_string()))?;
        file.write_all(&framed)
            .map_err(|error| SnapshotError::Io(error.to_string()))?;
        file.sync_all()
            .map_err(|error| SnapshotError::Io(error.to_string()))?;
    }
    // The rename is the commit point: before it the old snapshot stands, after
    // it the new one does, and never a mixture of the two.
    fs::rename(&temp, snapshot_path(root)).map_err(|error| SnapshotError::Io(error.to_string()))?;
    // Make the rename itself durable, so a crash cannot leave the directory
    // entry pointing at a file that lost its contents.
    if let Ok(directory) = File::open(root) {
        let _ = directory.sync_all();
    }
    Ok(())
}

fn encode(payload: &[u8]) -> Vec<u8> {
    let mut framed = Vec::with_capacity(HEADER_LEN + payload.len());
    framed.extend_from_slice(SNAPSHOT_MAGIC);
    framed.extend_from_slice(&SNAPSHOT_VERSION.to_le_bytes());
    framed.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    framed.extend_from_slice(&crc32(payload).to_le_bytes());
    framed.extend_from_slice(payload);
    framed
}

fn decode(bytes: &[u8]) -> Result<DomainSnapshot, SnapshotError> {
    if bytes.len() < HEADER_LEN {
        return Err(SnapshotError::Corrupt("shorter than the header".into()));
    }
    if &bytes[..8] != SNAPSHOT_MAGIC {
        return Err(SnapshotError::Corrupt("bad magic".into()));
    }
    let version = u32::from_le_bytes(bytes[8..12].try_into().expect("4 bytes"));
    let payload_len = u64::from_le_bytes(bytes[12..20].try_into().expect("8 bytes"));
    let stored_crc = u32::from_le_bytes(bytes[20..24].try_into().expect("4 bytes"));

    if version != SNAPSHOT_VERSION {
        return Err(SnapshotError::Corrupt(format!(
            "unsupported version {version}"
        )));
    }
    if payload_len > MAX_SNAPSHOT_LEN {
        return Err(SnapshotError::Corrupt(format!(
            "implausible payload length {payload_len}"
        )));
    }
    // A torn tail fails here: the frame claims more bytes than the file holds.
    let body = bytes
        .get(HEADER_LEN..HEADER_LEN + payload_len as usize)
        .ok_or_else(|| SnapshotError::Corrupt("truncated body".into()))?;
    if crc32(body) != stored_crc {
        return Err(SnapshotError::Corrupt("checksum mismatch".into()));
    }
    let snapshot: DomainSnapshot =
        serde_json::from_slice(body).map_err(|error| SnapshotError::Corrupt(error.to_string()))?;
    if snapshot.version != SNAPSHOT_VERSION {
        return Err(SnapshotError::Corrupt(format!(
            "payload version {} disagrees with the frame",
            snapshot.version
        )));
    }
    Ok(snapshot)
}

/// CRC-32/ISO-HDLC, hand-rolled so persistence needs no dependency.
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain_state::DomainRegistry;
    use tempfile::TempDir;

    fn snapshot() -> DomainSnapshot {
        let registry = DomainRegistry::new(10);
        DomainSnapshot {
            version: SNAPSHOT_VERSION,
            watermark: Some(EventId::new()),
            registry: registry.export(),
            runs: Vec::new(),
            settings: Some(SettingsSnapshot::default()),
            automations: Some(crate::automations::AutomationState::current()),
        }
    }

    #[test]
    fn a_written_snapshot_reads_back_identically() {
        let dir = TempDir::new().unwrap();
        assert_eq!(read_snapshot(dir.path()).unwrap(), None);

        let written = snapshot();
        write_snapshot(dir.path(), &written).unwrap();
        assert_eq!(read_snapshot(dir.path()).unwrap(), Some(written));
    }

    #[test]
    fn writing_replaces_atomically_and_leaves_no_temp_file() {
        let dir = TempDir::new().unwrap();
        let first = snapshot();
        write_snapshot(dir.path(), &first).unwrap();
        let second = {
            let mut second = snapshot();
            second.watermark = None;
            second
        };
        write_snapshot(dir.path(), &second).unwrap();

        assert_eq!(read_snapshot(dir.path()).unwrap(), Some(second));
        assert!(
            !dir.path()
                .join(format!("{SNAPSHOT_FILE}.{TEMP_SUFFIX}"))
                .exists(),
            "the staging file must be renamed, not left behind"
        );
    }

    #[test]
    fn a_truncated_snapshot_is_rejected_rather_than_half_read() {
        let dir = TempDir::new().unwrap();
        write_snapshot(dir.path(), &snapshot()).unwrap();
        let path = snapshot_path(dir.path());
        let full = fs::read(&path).unwrap();

        // A crash between `write` and `rename` would leave a short temp file,
        // never a short live one; but a torn file on disk must still be caught
        // by the length check instead of deserializing into nonsense.
        fs::write(&path, &full[..full.len() - 1]).unwrap();
        assert!(matches!(
            read_snapshot(dir.path()),
            Err(SnapshotError::Corrupt(_))
        ));
    }

    #[test]
    fn a_bit_flip_in_the_body_is_caught_by_the_checksum() {
        let dir = TempDir::new().unwrap();
        write_snapshot(dir.path(), &snapshot()).unwrap();
        let path = snapshot_path(dir.path());
        let mut bytes = fs::read(&path).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0x01;
        fs::write(&path, &bytes).unwrap();
        assert!(matches!(
            read_snapshot(dir.path()),
            Err(SnapshotError::Corrupt(_))
        ));
    }

    #[test]
    fn a_missing_directory_reads_as_no_snapshot() {
        let dir = TempDir::new().unwrap();
        let missing = dir.path().join("does-not-exist");
        assert_eq!(read_snapshot(&missing).unwrap(), None);
    }
}
