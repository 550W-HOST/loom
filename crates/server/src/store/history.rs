//! The conversation, as rows in the store.
//!
//! This is the same model the in-memory projection already uses — a
//! [`RowSource`] and a provider event, numbered by a sequence that is assigned
//! once — written down instead of kept. Nothing here invents a second message
//! shape: a row that goes in comes back out identical, which is what lets the
//! projection, the search index and the API all read one thing.
//!
//! Identity rules, and why they are here rather than in SQL:
//!
//! * **`seq` is assigned on insert and never recomputed.** It is what a client's
//!   cursor names. Deriving it from a row's position in a list is the bug the
//!   relay's retained-window index had.
//! * **`revision` only moves forward, and only a rebuild moves it.** A live row
//!   appended to the current conversation keeps the revision: the client's
//!   cursor is still a position in the same numbering.
//! * **A rebuild replaces the replayed rows and keeps loom's own.** What an
//!   agent replayed is the conversation; a provider error or a recovery
//!   diagnostic is loom's own fact about the turn, and replacing the baseline
//!   must not erase it.

use loom_domain::{HostId, ProviderEvent, RunId, ThreadId};
use rusqlite::{params, Connection, OptionalExtension};

use super::{Store, StoreError};
use crate::history_cache::{CacheBinding, RowSource};

/// One stored row, as the projection wants it.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredRow {
    /// The position a client's cursor names.
    pub seq: u64,
    /// Where the frame came from.
    pub source: RowSource,
    /// The frame itself.
    pub event: ProviderEvent,
}

/// What is known about a thread's stored conversation.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredHistory {
    /// What the stored conversation is bound to, when a session is known.
    pub binding: Option<CacheBinding>,
    /// How many rebuilds this conversation has had. Only moves forward.
    pub revision: u64,
    /// When the last successful sync finished.
    pub synced_at_ms: Option<u64>,
    /// Why the last attempt did not finish, when it did not.
    pub last_error: Option<String>,
}

/// The `source_kind` token a [`RowSource`] is stored as.
fn source_kind(source: &RowSource) -> &'static str {
    match source {
        RowSource::Message { .. } => "message",
        RowSource::Run { .. } => "run",
        RowSource::Replayed => "replayed",
    }
}

fn source_run_id(source: &RowSource) -> Option<String> {
    match source {
        RowSource::Run { run_id, .. } => Some(run_id.to_string()),
        _ => None,
    }
}

fn source_at_ms(source: &RowSource) -> Option<i64> {
    match source {
        RowSource::Message { at_ms } | RowSource::Run { at_ms, .. } => {
            Some(i64::try_from(*at_ms).unwrap_or(i64::MAX))
        }
        RowSource::Replayed => None,
    }
}

/// Rebuilds a [`RowSource`] from its stored columns.
fn decode_source(
    kind: &str,
    run_id: Option<String>,
    at_ms: Option<i64>,
) -> Result<RowSource, StoreError> {
    let at = at_ms.map(|value| u64::try_from(value).unwrap_or(0));
    match kind {
        "message" => Ok(RowSource::Message {
            at_ms: at.unwrap_or(0),
        }),
        "run" => {
            let run_id = run_id.ok_or_else(|| {
                StoreError::new("a stored run row has no run id, so its turn is unknown")
            })?;
            let run_id = run_id.parse::<RunId>().map_err(|error| {
                StoreError::new(format!("a stored run row names run {run_id:?}: {error}"))
            })?;
            Ok(RowSource::Run {
                run_id,
                at_ms: at.unwrap_or(0),
            })
        }
        "replayed" => Ok(RowSource::Replayed),
        other => Err(StoreError::new(format!(
            "a stored row has an unknown source {other:?}"
        ))),
    }
}

impl Store {
    /// What is known about a thread's stored conversation.
    pub fn history(&self, thread_id: &ThreadId) -> Result<Option<StoredHistory>, StoreError> {
        let row = self
            .connection()
            .query_row(
                "SELECT provider_session_id, binding_agent, binding_cwd, binding_host_id,
                        revision, synced_at_ms, last_error
                 FROM thread_history WHERE thread_id = ?1",
                params![thread_id.to_string()],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, i64>(4)?,
                        row.get::<_, Option<i64>>(5)?,
                        row.get::<_, Option<String>>(6)?,
                    ))
                },
            )
            .optional()?;
        let Some((session, agent, cwd, host, revision, synced_at_ms, last_error)) = row else {
            return Ok(None);
        };

        // A binding is only a binding when every part of it is there: a session
        // id with no machine is not one this server can ask for, and reporting
        // half of it as a binding would invite exactly that.
        let binding = match (session, agent, cwd, host) {
            (Some(provider_session_id), Some(agent), Some(cwd), Some(host)) => Some(CacheBinding {
                host_id: host.parse::<HostId>().map_err(|error| {
                    StoreError::new(format!("a stored binding names host {host:?}: {error}"))
                })?,
                agent,
                provider_session_id,
                cwd,
            }),
            _ => None,
        };
        Ok(Some(StoredHistory {
            binding,
            revision: u64::try_from(revision).unwrap_or(0),
            synced_at_ms: synced_at_ms.map(|value| u64::try_from(value).unwrap_or(0)),
            last_error,
        }))
    }

    /// The stored conversation, oldest first.
    pub fn rows(&self, thread_id: &ThreadId) -> Result<Vec<StoredRow>, StoreError> {
        let connection = self.connection();
        let mut statement = connection.prepare(
            "SELECT seq, source_kind, source_run_id, source_at_ms, event_json
             FROM thread_history_row WHERE thread_id = ?1 ORDER BY seq",
        )?;
        let mut rows = statement.query(params![thread_id.to_string()])?;
        let mut stored = Vec::new();
        while let Some(row) = rows.next()? {
            let seq = u64::try_from(row.get::<_, i64>(0)?).unwrap_or(0);
            let kind: String = row.get(1)?;
            let run_id: Option<String> = row.get(2)?;
            let at_ms: Option<i64> = row.get(3)?;
            let json: String = row.get(4)?;
            let event: ProviderEvent = serde_json::from_str(&json).map_err(|error| {
                StoreError::new(format!("a stored row is not a provider event: {error}"))
            })?;
            stored.push(StoredRow {
                seq,
                source: decode_source(&kind, run_id, at_ms)?,
                event,
            });
        }
        Ok(stored)
    }

    /// How many rows a thread has stored.
    pub fn row_count(&self, thread_id: &ThreadId) -> Result<u64, StoreError> {
        let count: i64 = self.connection().query_row(
            "SELECT COUNT(*) FROM thread_history_row WHERE thread_id = ?1",
            params![thread_id.to_string()],
            |row| row.get(0),
        )?;
        Ok(u64::try_from(count).unwrap_or(0))
    }

    /// Appends one live row, returning the sequence it was given.
    ///
    /// The sequence is `MAX(seq) + 1` *inside the transaction*, so two writers
    /// cannot be handed the same one and a row's number never depends on where
    /// it sits in a list.
    pub fn append_row(
        &self,
        thread_id: &ThreadId,
        source: &RowSource,
        event: &ProviderEvent,
    ) -> Result<u64, StoreError> {
        let connection = self.connection();
        let transaction = connection.unchecked_transaction()?;
        self.ensure_header(&transaction, thread_id)?;
        let seq = take_seq(&transaction, thread_id, 1)?;
        insert_row(&transaction, thread_id, seq, source, event)?;
        transaction.commit()?;
        Ok(seq)
    }

    /// Replaces the replayed conversation with a freshly loaded one.
    ///
    /// One transaction: the replayed rows go, the new ones arrive numbered
    /// after whatever loom's own rows already hold, the binding is recorded and
    /// the revision moves. Loom's own rows (`run`, `message`) are untouched —
    /// they are not part of what the agent replayed, and a rebuild that erased a
    /// provider error would lose the one record of why a turn failed.
    ///
    /// A failure is a rolled-back transaction: the previous baseline is exactly
    /// as it was, and `last_error` is not the caller's to fake.
    pub fn replace_replayed(
        &self,
        thread_id: &ThreadId,
        binding: &CacheBinding,
        events: &[ProviderEvent],
        at_ms: u64,
    ) -> Result<u64, StoreError> {
        let connection = self.connection();
        let transaction = connection.unchecked_transaction()?;
        transaction.execute(
            "INSERT INTO thread_history (thread_id) VALUES (?1)
             ON CONFLICT(thread_id) DO NOTHING",
            params![thread_id.to_string()],
        )?;
        transaction.execute(
            "DELETE FROM thread_history_row WHERE thread_id = ?1 AND source_kind = 'replayed'",
            params![thread_id.to_string()],
        )?;
        let first = take_seq(&transaction, thread_id, events.len())?;
        for (offset, event) in events.iter().enumerate() {
            let seq = first + offset as u64;
            insert_row(&transaction, thread_id, seq, &RowSource::Replayed, event)?;
        }
        transaction.execute(
            "UPDATE thread_history
                SET provider_session_id = ?2, binding_agent = ?3, binding_cwd = ?4,
                    binding_host_id = ?5, revision = revision + 1,
                    synced_at_ms = ?6, last_error = NULL
              WHERE thread_id = ?1",
            params![
                thread_id.to_string(),
                binding.provider_session_id,
                binding.agent,
                binding.cwd,
                binding.host_id.to_string(),
                i64::try_from(at_ms).unwrap_or(i64::MAX),
            ],
        )?;
        let revision: i64 = transaction.query_row(
            "SELECT revision FROM thread_history WHERE thread_id = ?1",
            params![thread_id.to_string()],
            |row| row.get(0),
        )?;
        transaction.commit()?;
        Ok(u64::try_from(revision).unwrap_or(0))
    }

    /// Records that the last attempt to sync a thread failed.
    ///
    /// The stored conversation is left alone: an old baseline plus "here is why
    /// it may be old" is a different answer from an empty thread, and the one a
    /// reader can act on.
    pub fn record_sync_failure(
        &self,
        thread_id: &ThreadId,
        reason: &str,
    ) -> Result<(), StoreError> {
        let connection = self.connection();
        let transaction = connection.unchecked_transaction()?;
        self.ensure_header(&transaction, thread_id)?;
        transaction.execute(
            "UPDATE thread_history SET last_error = ?2 WHERE thread_id = ?1",
            params![thread_id.to_string(), reason],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Removes a thread's stored conversation.
    ///
    /// The rows go with the header in one transaction; nothing of a deleted
    /// thread is left behind for a later read to resurrect.
    pub fn delete_thread(&self, thread_id: &ThreadId) -> Result<(), StoreError> {
        let connection = self.connection();
        let transaction = connection.unchecked_transaction()?;
        transaction.execute(
            "DELETE FROM thread_history_row WHERE thread_id = ?1",
            params![thread_id.to_string()],
        )?;
        transaction.execute(
            "DELETE FROM thread_history WHERE thread_id = ?1",
            params![thread_id.to_string()],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Makes sure a header row exists, so every stored row belongs to a thread
    /// the store knows about.
    fn ensure_header(
        &self,
        transaction: &Connection,
        thread_id: &ThreadId,
    ) -> Result<(), StoreError> {
        transaction.execute(
            "INSERT INTO thread_history (thread_id) VALUES (?1)
             ON CONFLICT(thread_id) DO NOTHING",
            params![thread_id.to_string()],
        )?;
        Ok(())
    }
}

/// Reserves `count` sequence numbers for a thread, inside the caller's
/// transaction.
///
/// The counter is what makes "a number is never reused" hold across a rebuild:
/// `MAX(seq)` would hand back the numbers a deleted replay used to hold.
fn take_seq(
    transaction: &Connection,
    thread_id: &ThreadId,
    count: usize,
) -> Result<u64, StoreError> {
    let current: i64 = transaction.query_row(
        "SELECT next_seq FROM thread_history WHERE thread_id = ?1",
        params![thread_id.to_string()],
        |row| row.get(0),
    )?;
    let reserved = u64::try_from(current).unwrap_or(1);
    let next = reserved.saturating_add(count as u64);
    transaction.execute(
        "UPDATE thread_history SET next_seq = ?2 WHERE thread_id = ?1",
        params![
            thread_id.to_string(),
            i64::try_from(next).unwrap_or(i64::MAX)
        ],
    )?;
    Ok(reserved)
}

fn insert_row(
    transaction: &Connection,
    thread_id: &ThreadId,
    seq: u64,
    source: &RowSource,
    event: &ProviderEvent,
) -> Result<(), StoreError> {
    let json = serde_json::to_string(event)
        .map_err(|error| StoreError::new(format!("a provider event did not serialize: {error}")))?;
    transaction.execute(
        "INSERT INTO thread_history_row
            (thread_id, seq, source_kind, source_run_id, source_at_ms, event_json)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            thread_id.to_string(),
            i64::try_from(seq).unwrap_or(i64::MAX),
            source_kind(source),
            source_run_id(source),
            source_at_ms(source),
            json,
        ],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use loom_domain::{ThreadEventItem, UserContent};

    fn thread() -> ThreadId {
        ThreadId::mint()
    }

    fn binding(agent: &str) -> CacheBinding {
        CacheBinding {
            host_id: HostId::mint(),
            agent: agent.to_owned(),
            provider_session_id: "acp-session-1".to_owned(),
            cwd: "/srv/project".to_owned(),
        }
    }

    fn message(text: &str) -> ProviderEvent {
        ProviderEvent::ItemStarted {
            item: ThreadEventItem::UserMessage {
                id: format!("user-{text}"),
                content: vec![UserContent::Text {
                    text: text.to_owned(),
                }],
                client_request_id: None,
                parent_tool_call_id: None,
            },
            provider_thread_id: "acp-session-1".to_owned(),
        }
    }

    fn identity() -> ProviderEvent {
        ProviderEvent::ThreadIdentity {
            provider_thread_id: "acp-session-1".to_owned(),
        }
    }

    #[test]
    fn a_row_comes_back_exactly_as_it_went_in() {
        let store = Store::open_in_memory().unwrap();
        let thread_id = thread();
        let source = RowSource::Run {
            run_id: RunId::mint(),
            at_ms: 1_700_000_000_000,
        };
        let event = message("hello");

        let seq = store.append_row(&thread_id, &source, &event).unwrap();
        assert_eq!(seq, 1);

        let rows = store.rows(&thread_id).unwrap();
        assert_eq!(
            rows,
            vec![StoredRow {
                seq: 1,
                source,
                event,
            }]
        );
    }

    #[test]
    fn sequences_are_assigned_in_order_and_keep_the_thread_open() {
        let store = Store::open_in_memory().unwrap();
        let thread_id = thread();
        for index in 1..=3 {
            let seq = store
                .append_row(
                    &thread_id,
                    &RowSource::Message { at_ms: index },
                    &message(&index.to_string()),
                )
                .unwrap();
            assert_eq!(seq, index, "the next row takes the next number");
        }
        assert_eq!(store.row_count(&thread_id).unwrap(), 3);
    }

    /// A rebuild replaces what the agent replayed and nothing else: loom's own
    /// rows (a prompt, a provider error) are not the agent's to erase.
    #[test]
    fn a_rebuild_replaces_the_replay_and_keeps_looms_own_rows() {
        let store = Store::open_in_memory().unwrap();
        let thread_id = thread();
        let binding = binding("pi");
        store
            .append_row(
                &thread_id,
                &RowSource::Message { at_ms: 10 },
                &message("a prompt"),
            )
            .unwrap();
        store
            .replace_replayed(
                &thread_id,
                &binding,
                &[message("first"), message("second")],
                20,
            )
            .unwrap();

        let first_revision = store.history(&thread_id).unwrap().unwrap();
        assert_eq!(first_revision.revision, 1);
        assert_eq!(first_revision.binding.as_ref(), Some(&binding));
        assert_eq!(first_revision.synced_at_ms, Some(20));
        assert_eq!(store.rows(&thread_id).unwrap().len(), 3);

        // A second rebuild: the prompt stays, the old replay is gone.
        let revision = store
            .replace_replayed(&thread_id, &binding, &[identity()], 30)
            .unwrap();
        assert_eq!(revision, 2, "a rebuild moves the revision forward");
        let rows = store.rows(&thread_id).unwrap();
        assert_eq!(rows.len(), 2, "the prompt plus the new replay: {rows:?}");
        assert_eq!(rows[0].event, message("a prompt"));
        assert_eq!(rows[1].source, RowSource::Replayed);
        let sequences: Vec<u64> = rows.iter().map(|row| row.seq).collect();
        assert_eq!(
            sequences,
            vec![1, 4],
            "numbers are never reused: {sequences:?}"
        );
    }

    /// A live row appended to the current conversation keeps the revision: the
    /// client's cursor is still a position in the same numbering.
    #[test]
    fn an_appended_row_does_not_move_the_revision() {
        let store = Store::open_in_memory().unwrap();
        let thread_id = thread();
        let binding = binding("pi");
        store
            .replace_replayed(&thread_id, &binding, &[identity()], 10)
            .unwrap();
        store
            .append_row(
                &thread_id,
                &RowSource::Message { at_ms: 11 },
                &message("later"),
            )
            .unwrap();
        assert_eq!(store.history(&thread_id).unwrap().unwrap().revision, 1);
    }

    #[test]
    fn a_failed_sync_keeps_the_baseline_and_records_why() {
        let store = Store::open_in_memory().unwrap();
        let thread_id = thread();
        let binding = binding("pi");
        store
            .replace_replayed(&thread_id, &binding, &[identity()], 10)
            .unwrap();
        store
            .record_sync_failure(&thread_id, "the agent is offline")
            .unwrap();

        let history = store.history(&thread_id).unwrap().unwrap();
        assert_eq!(history.revision, 1, "the baseline is untouched");
        assert_eq!(history.synced_at_ms, Some(10));
        assert_eq!(history.last_error.as_deref(), Some("the agent is offline"));
        assert_eq!(store.rows(&thread_id).unwrap().len(), 1);
    }

    #[test]
    fn deleting_a_thread_removes_its_rows_and_its_header() {
        let store = Store::open_in_memory().unwrap();
        let thread_id = thread();
        let other = thread();
        store
            .append_row(
                &thread_id,
                &RowSource::Message { at_ms: 1 },
                &message("mine"),
            )
            .unwrap();
        store
            .append_row(&other, &RowSource::Message { at_ms: 1 }, &message("theirs"))
            .unwrap();

        store.delete_thread(&thread_id).unwrap();
        assert!(store.history(&thread_id).unwrap().is_none());
        assert_eq!(store.row_count(&thread_id).unwrap(), 0);
        assert_eq!(
            store.row_count(&other).unwrap(),
            1,
            "another thread's conversation is untouched"
        );
    }

    /// A store whose binding is incomplete reports no binding at all: half of
    /// one is what would let a load be asked for a session loom cannot place.
    #[test]
    fn an_incomplete_binding_reads_as_no_binding() {
        let store = Store::open_in_memory().unwrap();
        let thread_id = thread();
        store
            .connection()
            .execute(
                "INSERT INTO thread_history (thread_id, provider_session_id, binding_agent)
                 VALUES (?1, 'acp-1', 'pi')",
                params![thread_id.to_string()],
            )
            .unwrap();
        let history = store.history(&thread_id).unwrap().unwrap();
        assert_eq!(history.binding, None);
    }
}
