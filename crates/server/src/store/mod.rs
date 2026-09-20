//! The server's embedded store.
//!
//! One database file under the data directory is where the server keeps what
//! must outlive it. Phase one puts the **conversation** there: the normalized
//! events an agent replayed, in the order it replayed them, so a thread can be
//! read after the worker that owns its session has gone away. The relay's log
//! and the entity snapshot keep their jobs until their own phases move them.
//!
//! The engine is [`turso`], an in-process SQLite-compatible database written in
//! Rust. Its driver is async; this module drives those futures with
//! [`futures_executor::block_on`] so the store's own API stays synchronous,
//! which is what every caller is: an HTTP read, the store's writer thread, a
//! test. turso needs no Tokio runtime to make progress, and driving it on the
//! calling thread is the same shape the previous embedded engine had.
//!
//! Two rules are the server's, not the database's, and both are about identity:
//!
//! * **A row's sequence is reserved by the publisher and never recomputed.** It
//!   is the position a client's cursor names, so deriving it from an offset in a
//!   list — which is what the relay's retained-window index did — is the bug this
//!   store exists to remove.
//! * **A thread's revision only moves forward.** A rebuild of the replayed
//!   conversation bumps it, and a client holding a cursor from another revision
//!   is told to refetch rather than reading that cursor as a position here.
//!
//! Opening the store is part of starting the server. A store that cannot be
//! opened, or that was written by a newer build than this one, fails startup:
//! there is deliberately no fallback to running without persistence, because a
//! server that silently forgets everything is worse than one that refuses to
//! start.

mod entities;
mod history;
mod relay_backend;
mod relay_log;
mod schema;
mod seq;
mod writer;

pub use history::{StoredHistory, StoredRow};
pub use relay_backend::StoreBackend;
pub use seq::SeqAllocator;
pub use writer::{StoreWriter, WrittenRows};

use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::Duration;

use turso::{Builder, Connection, Row, Value};

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

impl From<turso::Error> for StoreError {
    fn from(error: turso::Error) -> Self {
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

/// How long a statement waits for a lock another connection holds.
///
/// The server is the only writer, so waiting is almost never needed; the bound
/// exists so a stray reader cannot turn into a hang.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Drives one turso future to completion on this thread.
///
/// turso's IO is its own: the futures make progress without a Tokio runtime, so
/// this works from an HTTP worker, from the writer thread, and from a test.
pub(crate) fn block_on<F: Future>(future: F) -> F::Output {
    futures_executor::block_on(future)
}

/// The text in a column, or an error naming what was there instead.
pub(crate) fn column_text(row: &Row, index: usize) -> Result<String, StoreError> {
    match row.get_value(index)? {
        Value::Text(text) => Ok(text),
        other => Err(StoreError::new(format!(
            "column {index} was {other:?}, not text"
        ))),
    }
}

/// The text in a column, or `None` when it is null.
pub(crate) fn column_optional_text(row: &Row, index: usize) -> Result<Option<String>, StoreError> {
    match row.get_value(index)? {
        Value::Text(text) => Ok(Some(text)),
        Value::Null => Ok(None),
        other => Err(StoreError::new(format!(
            "column {index} was {other:?}, not text or null"
        ))),
    }
}

/// The bytes in a column.
pub(crate) fn column_blob(row: &Row, index: usize) -> Result<Vec<u8>, StoreError> {
    match row.get_value(index)? {
        Value::Blob(bytes) => Ok(bytes),
        other => Err(StoreError::new(format!(
            "column {index} was {other:?}, not bytes"
        ))),
    }
}

/// The integer in a column.
pub(crate) fn column_integer(row: &Row, index: usize) -> Result<i64, StoreError> {
    match row.get_value(index)? {
        Value::Integer(value) => Ok(value),
        other => Err(StoreError::new(format!(
            "column {index} was {other:?}, not an integer"
        ))),
    }
}

/// The integer in a column, or `None` when it is null.
pub(crate) fn column_optional_integer(row: &Row, index: usize) -> Result<Option<i64>, StoreError> {
    match row.get_value(index)? {
        Value::Integer(value) => Ok(Some(value)),
        Value::Null => Ok(None),
        other => Err(StoreError::new(format!(
            "column {index} was {other:?}, not an integer or null"
        ))),
    }
}

/// The server's store: one database, opened once, migrated on the way in.
#[derive(Debug)]
pub struct Store {
    /// Kept alive for as long as the connection is: it owns the engine's
    /// background IO and the file handle behind every connection to it.
    _database: turso::Database,
    connection: Connection,
    path: PathBuf,
    /// This store's identity, minted when the file is created and kept from then
    /// on. See [`Store::instance`].
    instance: String,
    /// Whether the process that had this file last stopped without finishing.
    unclean_stop: bool,
}

/// The `store_meta` key that says the last process stopped on purpose.
const CLEAN_STOP: &str = "clean_stop";

/// What the last stop left behind: `Some(true)` a finished stop, `Some(false)`
/// one that did not finish, `None` a file no stop has touched yet.
fn clean_stop_of(connection: &Connection) -> Result<Option<bool>, StoreError> {
    let mut rows =
        block_on(connection.query("SELECT value FROM store_meta WHERE key = ?1", (CLEAN_STOP,)))?;
    Ok(match block_on(rows.next())? {
        Some(row) => Some(column_text(&row, 0)? == "1"),
        None => None,
    })
}

/// Records that this process is running, so a stop that never finishes is
/// distinguishable from one that did.
fn mark_running(connection: &Connection) -> Result<(), StoreError> {
    block_on(connection.execute(
        "INSERT INTO store_meta (key, value) VALUES (?1, '0')
         ON CONFLICT(key) DO UPDATE SET value = '0'",
        (CLEAN_STOP,),
    ))?;
    Ok(())
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
        let location = path.to_str().ok_or_else(|| {
            StoreError::new(format!(
                "the store path {} is not valid UTF-8",
                path.display()
            ))
        })?;
        let database = block_on(Builder::new_local(location).build()).map_err(|error| {
            StoreError::new(format!(
                "could not open the store {}: {error}",
                path.display()
            ))
        })?;
        let connection = database.connect()?;
        Self::configure(&connection, true)?;
        schema::migrate(&connection)?;
        let instance = instance_of(&connection)?;
        // A file this process is the only writer of: reading the flag says how
        // the *previous* process left it, and writing it says this one is
        // running. A store that was never opened before is not behind anything.
        let unclean_stop = clean_stop_of(&connection)? == Some(false);
        mark_running(&connection)?;
        Ok(Self {
            _database: database,
            connection,
            path,
            instance,
            unclean_stop,
        })
    }

    /// An in-memory store, for tests that need the schema and not the file.
    pub fn open_in_memory() -> Result<Self, StoreError> {
        let database = block_on(Builder::new_local(":memory:").build())?;
        let connection = database.connect()?;
        // An in-memory store has no file to write ahead of, and the engine says
        // so by answering `memory`; asking for WAL there is a mistake, not a
        // failure to report.
        Self::configure(&connection, false)?;
        schema::migrate(&connection)?;
        let instance = instance_of(&connection)?;
        mark_running(&connection)?;
        Ok(Self {
            _database: database,
            connection,
            path: PathBuf::from(":memory:"),
            instance,
            unclean_stop: false,
        })
    }

    /// Where this store lives, for diagnostics.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The identity of the numbering every cursor in this store names.
    ///
    /// Minted with the file and kept, so it survives a restart: a client
    /// holding `(instance, revision)` can tell "this conversation was rebuilt"
    /// from "the server restarted", which a bare revision cannot.
    pub fn instance(&self) -> &str {
        &self.instance
    }

    /// The connection itself, for the typed modules built on it.
    pub fn connection(&self) -> &Connection {
        &self.connection
    }

    /// The schema version the file is at.
    pub fn schema_version(&self) -> Result<i64, StoreError> {
        schema::version(&self.connection)
    }

    /// Whether the process that had this file last stopped without finishing.
    ///
    /// A store that was killed can be missing the rows a published-but-unwritten
    /// conversation held, and nothing in the file can say *what* is missing. So
    /// the file says the one thing it does know: the last stop did not finish,
    /// and what is stored may be behind what happened.
    pub fn recovered_from_unclean_stop(&self) -> bool {
        self.unclean_stop
    }

    /// Records that this process is stopping on purpose.
    ///
    /// Written last, after everything else is on disk: a stop that fails before
    /// this leaves the file marked as unfinished, which is the conservative
    /// reading and the honest one.
    pub fn mark_clean_stop(&self) -> Result<(), StoreError> {
        block_on(self.connection.execute(
            "UPDATE store_meta SET value = '1' WHERE key = ?1",
            (CLEAN_STOP,),
        ))?;
        Ok(())
    }

    /// Marks every stored conversation that had a baseline as possibly behind.
    ///
    /// Only threads that were once complete are marked: a thread that never
    /// synced already reads as partial, and there is nothing to be behind.
    /// Returns how many were marked.
    pub fn mark_stored_history_behind(&self, reason: &str) -> Result<usize, StoreError> {
        let marked = block_on(self.connection.execute(
            "UPDATE thread_history SET last_error = ?1 WHERE synced_at_ms IS NOT NULL",
            (reason,),
        ))?;
        Ok(marked as usize)
    }

    fn configure(connection: &Connection, wal: bool) -> Result<(), StoreError> {
        // WAL keeps a reader from blocking the writer; `NORMAL` is the durability
        // level that pairs with it (a crash can lose the last commits, which is
        // exactly what the plan already promises for uncommitted history).
        //
        // Setting `journal_mode` answers with the mode it settled on, so the
        // answer is read rather than discarded.
        if wal {
            let rows = block_on(connection.pragma_update("journal_mode", "WAL"))?;
            let mode = rows
                .first()
                .map(|row| column_text(row, 0))
                .transpose()?
                .unwrap_or_default();
            if !mode.eq_ignore_ascii_case("wal") {
                return Err(StoreError::new(format!(
                    "the store could not be put in WAL mode (it answered {mode:?})"
                )));
            }
        }
        block_on(connection.pragma_update("synchronous", "NORMAL"))?;
        block_on(connection.pragma_update("foreign_keys", "ON"))?;
        connection.busy_timeout(BUSY_TIMEOUT)?;
        Ok(())
    }
}

/// Reads the store's identity, minting it on first use.
///
/// `DO NOTHING` on conflict is what makes it idempotent, and the read-back is
/// what makes concurrent first opens agree on one value instead of each keeping
/// the id it tried to write.
fn instance_of(connection: &Connection) -> Result<String, StoreError> {
    block_on(connection.execute(
        "INSERT INTO store_meta (key, value) VALUES ('instance', ?1)
         ON CONFLICT(key) DO NOTHING",
        (loom_relay::EventId::new().to_string(),),
    ))?;
    let mut rows =
        block_on(connection.query("SELECT value FROM store_meta WHERE key = 'instance'", ()))?;
    match block_on(rows.next())? {
        Some(row) => column_text(&row, 0),
        None => Err(StoreError::new("the store has no identity row")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A store that was killed says so, and one that stopped on purpose does
    /// not: the difference is the only thing that can tell a reader its
    /// conversation may be missing a tail.
    #[test]
    fn a_store_knows_whether_the_last_stop_finished() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("loom.db");

        let store = Store::open(&path).unwrap();
        assert!(
            !store.recovered_from_unclean_stop(),
            "a file nothing has written yet is not behind anything"
        );
        drop(store);

        // The first process went away without finishing.
        let store = Store::open(&path).unwrap();
        assert!(store.recovered_from_unclean_stop());
        drop(store);

        // A second unclean open is still unclean: only a stop that finishes
        // clears the mark.
        let store = Store::open(&path).unwrap();
        assert!(store.recovered_from_unclean_stop());
        store.mark_clean_stop().unwrap();
        drop(store);

        let store = Store::open(&path).unwrap();
        assert!(
            !store.recovered_from_unclean_stop(),
            "a stop that finished is not a surprise"
        );
    }

    #[test]
    fn opening_creates_the_store_and_is_idempotent() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("loom.db");
        let instance;
        {
            let store = Store::open(&path).unwrap();
            assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
            let mut rows = block_on(store.connection().query("PRAGMA journal_mode", ())).unwrap();
            let row = block_on(rows.next()).unwrap().expect("a journal mode");
            assert_eq!(column_text(&row, 0).unwrap(), "wal");
            instance = store.instance().to_owned();
        }
        assert!(path.exists(), "the store is a real file on disk");

        // Reopening an existing store is not a migration and must not fail.
        let again = Store::open(&path).unwrap();
        assert_eq!(again.schema_version().unwrap(), SCHEMA_VERSION);
        assert_eq!(
            again.instance(),
            instance,
            "the identity is the file's, not the process's: a client's cursor \
             survives a restart"
        );
    }

    /// Two stores are two numberings, so the identities differ — a cursor from
    /// one must not be read as a position in the other.
    #[test]
    fn separate_stores_have_separate_identities() {
        let first = Store::open_in_memory().unwrap();
        let second = Store::open_in_memory().unwrap();
        assert_ne!(first.instance(), second.instance());
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
            block_on(
                store
                    .connection()
                    .pragma_update("user_version", SCHEMA_VERSION + 1),
            )
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
