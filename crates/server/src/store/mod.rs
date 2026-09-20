//! The server's embedded store.
//!
//! One SQLite file under the data directory is where the server keeps what must
//! outlive it. Phase one puts the **conversation** there: the normalized events
//! an agent replayed, in the order it replayed them, so a thread can be read
//! after the worker that owns its session has gone away. The relay's log and
//! the entity snapshot keep their jobs until their own phases move them.
//!
//! Two rules are the server's, not the database's, and both are about identity:
//!
//! * **A row's sequence is assigned when it is written and never recomputed.**
//!   It is the position a client's cursor names, so deriving it from an offset
//!   in a list — which is what the relay's retained-window index did — is the
//!   bug this store exists to remove.
//! * **A thread's revision only moves forward.** A rebuild of the replayed
//!   conversation bumps it, and a client holding a cursor from another revision
//!   is told to refetch rather than reading that cursor as a position here.
//!
//! Opening the store is part of starting the server. A store that cannot be
//! opened, or that was written by a newer build than this one, fails startup:
//! there is deliberately no fallback to running without persistence, because a
//! server that silently forgets everything is worse than one that refuses to
//! start.

mod history;
mod schema;
mod writer;

pub use history::{StoredHistory, StoredRow};
pub use writer::StoreWriter;

use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::Connection;

pub use schema::SCHEMA_VERSION;

/// Why the store could not be used.
#[derive(Debug)]
pub struct StoreError {
    message: String,
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for StoreError {}

impl From<rusqlite::Error> for StoreError {
    fn from(error: rusqlite::Error) -> Self {
        Self {
            message: error.to_string(),
        }
    }
}

impl StoreError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// How long a writer waits for a lock another connection holds.
///
/// The server is the only writer, so waiting is almost never needed; the bound
/// exists so a stray reader cannot turn into a hang.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// The server's store: one connection, opened once, migrated on the way in.
#[derive(Debug)]
pub struct Store {
    connection: Connection,
    path: PathBuf,
}

impl Store {
    /// Opens (or creates) the store at `path`, applying the schema.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                StoreError::new(format!(
                    "could not create the store's directory {}: {error}",
                    parent.display()
                ))
            })?;
        }
        let connection = Connection::open(&path).map_err(|error| {
            StoreError::new(format!(
                "could not open the store {}: {error}",
                path.display()
            ))
        })?;
        Self::configure(&connection, true)?;
        schema::migrate(&connection)?;
        Ok(Self { connection, path })
    }

    /// An in-memory store, for tests that need the schema and not the file.
    pub fn open_in_memory() -> Result<Self, StoreError> {
        let connection = Connection::open_in_memory()?;
        // An in-memory store has no file to write ahead of, and SQLite says so
        // by answering `memory`; asking for WAL there is a mistake, not a
        // failure to report.
        Self::configure(&connection, false)?;
        schema::migrate(&connection)?;
        Ok(Self {
            connection,
            path: PathBuf::from(":memory:"),
        })
    }

    /// Where this store lives, for diagnostics.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The connection itself, for the typed modules built on it.
    pub fn connection(&self) -> &Connection {
        &self.connection
    }

    /// The schema version the file is at.
    pub fn schema_version(&self) -> Result<i64, StoreError> {
        schema::version(&self.connection)
    }

    fn configure(connection: &Connection, wal: bool) -> Result<(), StoreError> {
        // WAL keeps a reader from blocking the writer; `NORMAL` is the durability
        // level that pairs with it (a crash can lose the last commits, which is
        // exactly what the plan already promises for uncommitted history).
        //
        // `journal_mode` answers with the mode it settled on, so it is read
        // rather than executed: `execute_batch` would discard the answer this
        // has to check.
        if wal {
            let mode: String =
                connection.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))?;
            if !mode.eq_ignore_ascii_case("wal") {
                return Err(StoreError::new(format!(
                    "the store could not be put in WAL mode (it answered {mode:?})"
                )));
            }
        }
        connection.execute_batch(
            "PRAGMA synchronous = NORMAL;
             PRAGMA foreign_keys = ON;",
        )?;
        connection.busy_timeout(BUSY_TIMEOUT)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opening_creates_the_store_and_is_idempotent() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("loom.db");
        {
            let store = Store::open(&path).unwrap();
            assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
            let mode: String = store
                .connection()
                .query_row("PRAGMA journal_mode", [], |row| row.get(0))
                .unwrap();
            assert_eq!(mode, "wal");
        }
        assert!(path.exists(), "the store is a real file on disk");

        // Reopening an existing store is not a migration and must not fail.
        let again = Store::open(&path).unwrap();
        assert_eq!(again.schema_version().unwrap(), SCHEMA_VERSION);
    }

    /// A store written by a newer build is refused rather than used: this build
    /// does not know what its rows mean, and reading them anyway is how a
    /// downgrade quietly corrupts history.
    #[test]
    fn a_store_from_a_newer_build_is_refused() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("loom.db");
        {
            let store = Store::open(&path).unwrap();
            store
                .connection()
                .pragma_update(None, "user_version", SCHEMA_VERSION + 1)
                .unwrap();
        }
        let error = Store::open(&path).expect_err("a newer schema is refused");
        assert!(
            error.to_string().contains("newer"),
            "the refusal says why: {error}"
        );
    }

    /// The store that cannot be opened fails startup. There is no path from
    /// here to "run without persistence".
    #[test]
    fn an_unusable_path_fails_loudly() {
        let dir = tempfile::TempDir::new().unwrap();
        let blocker = dir.path().join("not-a-directory");
        std::fs::write(&blocker, b"occupied").unwrap();

        let error = Store::open(blocker.join("loom.db")).expect_err("a file is not a directory");
        assert!(
            error.to_string().contains("could not create"),
            "the failure names the directory: {error}"
        );
    }

    /// A file that is not a store is refused instead of being overwritten: the
    /// operator pointed the server at something, and guessing what they meant is
    /// not this code's call.
    #[test]
    fn a_file_that_is_not_a_store_is_refused() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("loom.db");
        std::fs::write(&path, b"this is not a sqlite database, not even close").unwrap();

        assert!(
            Store::open(&path).is_err(),
            "a corrupt store must fail rather than be replaced"
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"this is not a sqlite database, not even close",
            "and it must be left exactly as it was"
        );
    }
}
