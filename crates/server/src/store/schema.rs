//! The store's schema and its migrations.
//!
//! The version lives in SQLite's own `user_version`, which moves with the same
//! transaction that changes the tables: there is no second place for "what shape
//! is this file" to disagree with the file.

use rusqlite::Connection;

use super::StoreError;

/// The schema this build writes and understands.
pub const SCHEMA_VERSION: i64 = 1;

/// The version the file currently holds.
pub fn version(connection: &Connection) -> Result<i64, StoreError> {
    Ok(connection.query_row("PRAGMA user_version", [], |row| row.get(0))?)
}

/// Brings the file up to [`SCHEMA_VERSION`], or refuses it.
///
/// A file from a *newer* build is an error, not something to open and hope
/// about: this build does not know what those columns mean, and a downgrade
/// that reads them anyway is how history gets quietly mangled.
pub fn migrate(connection: &Connection) -> Result<(), StoreError> {
    let current = version(connection)?;
    if current > SCHEMA_VERSION {
        return Err(StoreError::new(format!(
            "the store was written by a newer build (schema {current}, this build understands \
             {SCHEMA_VERSION}); refusing to open it"
        )));
    }
    if current == SCHEMA_VERSION {
        return Ok(());
    }

    // One transaction per step, with the version bump inside it: a process that
    // dies mid-migration comes back to either the old shape or the new one.
    let transaction = connection.unchecked_transaction()?;
    if current < 1 {
        transaction.execute_batch(V1)?;
    }
    transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    transaction.commit()?;
    Ok(())
}

/// Version 1: the conversation, as the agent replayed it.
///
/// `thread_history` is one row per thread — what the conversation is bound to
/// and which revision of it is stored. `thread_history_row` is the conversation
/// itself: the same provider events the in-memory projection consumes, in the
/// order they were written, each with the sequence a client's cursor names.
///
/// `seq` is assigned by the writer (`MAX(seq) + 1` in the same transaction) and
/// never recomputed. `source_kind` says where a row came from, which is what
/// lets a rebuild replace the replayed conversation without discarding the
/// diagnostics loom published itself.
const V1: &str = "
CREATE TABLE IF NOT EXISTS thread_history (
    thread_id           TEXT PRIMARY KEY,
    provider_session_id TEXT,
    binding_agent       TEXT,
    binding_cwd         TEXT,
    binding_host_id     TEXT,
    revision            INTEGER NOT NULL DEFAULT 0,
    synced_at_ms        INTEGER,
    last_error          TEXT
);

CREATE TABLE IF NOT EXISTS thread_history_row (
    thread_id     TEXT    NOT NULL,
    seq           INTEGER NOT NULL,
    source_kind   TEXT    NOT NULL,
    source_run_id TEXT,
    source_at_ms  INTEGER,
    event_json    TEXT    NOT NULL,
    PRIMARY KEY (thread_id, seq)
);

CREATE INDEX IF NOT EXISTS thread_history_row_run
    ON thread_history_row (source_run_id);
";
