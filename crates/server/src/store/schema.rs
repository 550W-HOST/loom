//! The store's schema and its migrations.
//!
//! The version lives in SQLite's own `user_version`, which moves with the same
//! transaction that changes the tables: there is no second place for "what shape
//! is this file" to disagree with the file.

use turso::Connection;

use super::{block_on, column_integer, StoreError};

/// The schema this build writes and understands.
pub const SCHEMA_VERSION: i64 = 4;

/// The version the file currently holds.
pub fn version(connection: &Connection) -> Result<i64, StoreError> {
    let mut rows = block_on(connection.query("PRAGMA user_version", ()))?;
    match block_on(rows.next())? {
        Some(row) => column_integer(&row, 0),
        // A store that answers nothing has no version yet, which is zero.
        None => Ok(0),
    }
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
    let transaction = block_on(connection.unchecked_transaction())?;
    if current < 1 {
        block_on(transaction.execute_batch(V1))?;
    }
    if current < 2 {
        block_on(transaction.execute_batch(V2))?;
    }
    if current < 3 {
        block_on(transaction.execute_batch(V3))?;
    }
    if current < 4 {
        block_on(transaction.execute_batch(V4))?;
    }
    block_on(transaction.pragma_update("user_version", SCHEMA_VERSION))?;
    block_on(transaction.commit())?;
    Ok(())
}

/// Version 4: the relay's frames, in a table.
///
/// The frames were an append-only file per shard: the thing a client resumes
/// from, the thing a worker's missed frames are caught up with, and the delta
/// recovery replays after the entity view's watermark. They are rows now, in the
/// store that already holds the conversations and the entity view, so one
/// database is the whole durable state and a transaction can span a frame and
/// the state it implies.
///
/// `event_id` is the identity *and* the order: a fixed-width Crockford base32 of
/// a `u128`, so its text order is the id order the in-memory backends filter by.
const V4: &str = "
CREATE TABLE IF NOT EXISTS relay_event (
    event_id      TEXT    PRIMARY KEY,
    shard         INTEGER NOT NULL,
    scope_kind    TEXT    NOT NULL,
    scope_id      TEXT,
    payload       BLOB    NOT NULL,
    created_at_ms INTEGER NOT NULL,
    origin        TEXT    NOT NULL
);

CREATE INDEX IF NOT EXISTS relay_event_shard
    ON relay_event (shard, event_id);

CREATE INDEX IF NOT EXISTS relay_event_age
    ON relay_event (shard, created_at_ms);
";

/// Version 3: the entity view, in tables.
///
/// The same entities the durable snapshot held — projects, threads, hosts,
/// environments, queued messages, interactions, sidebar sections, runs, settings
/// and automations — with the identity and the parent a lookup needs as columns
/// and the entity itself as the stored form. One table with a `kind` rather than
/// eleven near-identical ones: the columns would be the same in every one and so
/// would every query, and what the plan asks for is that the keys and lookup
/// fields be columns, which they are.
const V3: &str = "
CREATE TABLE IF NOT EXISTS entity (
    kind      TEXT NOT NULL,
    id        TEXT NOT NULL,
    parent_id TEXT,
    json      TEXT NOT NULL,
    PRIMARY KEY (kind, id)
);

CREATE INDEX IF NOT EXISTS entity_parent ON entity (kind, parent_id);

CREATE TABLE IF NOT EXISTS entity_meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
";

/// Version 2: the store's own identity.
///
/// A client's cursor is a position in a conversation, and it only means
/// something together with *which* numbering it is a position in. That used to
/// be an in-memory instance id, which a restart threw away — and a client that
/// then compared revisions across the restart read the new numbering as an
/// older one. The store is what outlives the process, so it is what mints and
/// keeps the id.
const V2: &str = "
CREATE TABLE IF NOT EXISTS store_meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
";

/// Version 1: the conversation, as the agent replayed it.
///
/// `thread_history` is one row per thread — what the conversation is bound to
/// and which revision of it is stored. `thread_history_row` is the conversation
/// itself: the same provider events the in-memory projection consumes, in the
/// order they were written, each with the sequence a client's cursor names.
///
/// `seq` is reserved by the publisher from the thread's `next_seq` counter and
/// never recomputed, and a deleted row's number is not handed out again. `source_kind` says where a row came from, which is what
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
    -- The sequence the next row this thread stores will get. It is a counter,
    -- not `MAX(seq)`: a rebuild deletes the replayed rows, and the numbers they
    -- held must not come back — a position in a conversation is never reused.
    next_seq            INTEGER NOT NULL DEFAULT 1,
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
