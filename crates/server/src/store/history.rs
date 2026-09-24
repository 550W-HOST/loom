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
//! * **`seq` is reserved by the publisher and never recomputed.** It is what a
//!   client's cursor names, and the publisher reserves it because the same
//!   number has to name the row in two places at once: the durable one here and
//!   the displayable one before the write lands. Deriving it from a row's
//!   position in a list is the bug the relay's retained-window index had.
//! * **`revision` only moves forward, and only a rebuild moves it.** A live row
//!   appended to the current conversation keeps the revision: the client's
//!   cursor is still a position in the same numbering.
//! * **A rebuild replaces the replayed rows and keeps loom's own.** What an
//!   agent replayed is the conversation; a provider error or a recovery
//!   diagnostic is loom's own fact about the turn, and replacing the baseline
//!   must not erase it.

use loom_domain::{HostId, ProviderEvent, RunId, ThreadId};
use rusqlite::{params, Connection};

use super::{
    column_integer, column_optional_integer, column_optional_text, column_text, Store, StoreError,
};
use crate::history_cache::{first_user_prompt_title, CacheBinding, RowSource};

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
    /// When the last successful provider replay finished.
    pub synced_at_ms: Option<u64>,
    /// The locally recorded rows are known to be complete for this session.
    pub local_complete: bool,
    /// The locally recorded rows may be missing a tail after an unclean stop.
    pub local_uncertain: bool,
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
        let mut statement = self.connection().prepare(
            "SELECT provider_session_id, binding_agent, binding_cwd, binding_host_id,
                    revision, synced_at_ms, last_error, local_complete, local_uncertain
             FROM thread_history WHERE thread_id = ?1",
        )?;
        let mut rows = statement.query((thread_id.to_string(),))?;
        let Some(row) = rows.next()? else {
            return Ok(None);
        };
        let session = column_optional_text(row, 0)?;
        let agent = column_optional_text(row, 1)?;
        let cwd = column_optional_text(row, 2)?;
        let host = column_optional_text(row, 3)?;
        let revision = column_integer(row, 4)?;
        let synced_at_ms = column_optional_integer(row, 5)?;
        let last_error = column_optional_text(row, 6)?;
        let local_complete = column_integer(row, 7)? != 0;
        let local_uncertain = column_integer(row, 8)? != 0;

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
            local_complete,
            local_uncertain,
            last_error,
        }))
    }

    /// The stored conversation, oldest first.
    pub fn rows(&self, thread_id: &ThreadId) -> Result<Vec<StoredRow>, StoreError> {
        let mut statement = self.connection().prepare(
            "SELECT seq, source_kind, source_run_id, source_at_ms, event_json
             FROM thread_history_row WHERE thread_id = ?1 ORDER BY seq",
        )?;
        let mut rows = statement.query((thread_id.to_string(),))?;
        let mut stored = Vec::new();
        while let Some(row) = rows.next()? {
            let seq = u64::try_from(column_integer(row, 0)?).unwrap_or(0);
            let kind = column_text(row, 1)?;
            let run_id = column_optional_text(row, 2)?;
            let at_ms = column_optional_integer(row, 3)?;
            let json = column_text(row, 4)?;
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

    /// Finds the first Loom-authored user prompt without loading the rest of a
    /// thread's history. Used for title fallback on existing threads.
    pub(crate) fn first_user_prompt_title(
        &self,
        thread_id: &ThreadId,
    ) -> Result<Option<String>, StoreError> {
        let mut statement = self.connection().prepare(
            "SELECT event_json FROM thread_history_row
             WHERE thread_id = ?1 AND source_kind = 'message'
             ORDER BY seq",
        )?;
        let mut rows = statement.query((thread_id.to_string(),))?;
        while let Some(row) = rows.next()? {
            let json = column_text(row, 0)?;
            let event: ProviderEvent = serde_json::from_str(&json).map_err(|error| {
                StoreError::new(format!("a stored row is not a provider event: {error}"))
            })?;
            if let Some(title) = first_user_prompt_title(&event) {
                return Ok(Some(title));
            }
        }
        Ok(None)
    }

    /// How many rows a thread has stored.
    pub fn row_count(&self, thread_id: &ThreadId) -> Result<u64, StoreError> {
        let mut statement = self
            .connection()
            .prepare("SELECT COUNT(*) FROM thread_history_row WHERE thread_id = ?1")?;
        let mut rows = statement.query((thread_id.to_string(),))?;
        let count = match rows.next()? {
            Some(row) => column_integer(row, 0)?,
            None => 0,
        };
        Ok(u64::try_from(count).unwrap_or(0))
    }

    /// The highest sequence a thread's stored conversation holds, or zero when
    /// it has none.
    ///
    /// A reader that only needs to know whether the stored conversation moved
    /// asks this instead of [`Store::rows`]: it is one indexed lookup, and it
    /// leaves the rows themselves — the expensive part of a read — on disk.
    pub fn stored_last_seq(&self, thread_id: &ThreadId) -> Result<u64, StoreError> {
        let mut statement = self
            .connection()
            .prepare("SELECT COALESCE(MAX(seq), 0) FROM thread_history_row WHERE thread_id = ?1")?;
        let mut rows = statement.query((thread_id.to_string(),))?;
        let last_seq = match rows.next()? {
            Some(row) => column_integer(row, 0)?,
            None => 0,
        };
        Ok(u64::try_from(last_seq).unwrap_or(0))
    }

    /// Appends one live row at the sequence its publisher reserved.
    ///
    /// The sequence arrives from [`Store::next_seq`]'s numbering rather than
    /// being derived here, because the publisher has already shown the row with
    /// that number: see [`crate::store::SeqAllocator`]. A number this thread
    /// already holds is refused by the primary key rather than overwriting it.
    pub fn append_row(
        &self,
        thread_id: &ThreadId,
        seq: u64,
        source: &RowSource,
        event: &ProviderEvent,
    ) -> Result<(), StoreError> {
        let transaction = self.connection().unchecked_transaction()?;
        self.ensure_header(&transaction, thread_id)?;
        insert_row(&transaction, thread_id, seq, source, event)?;
        bump_next_seq(&transaction, thread_id, seq)?;
        transaction.commit()?;
        Ok(())
    }

    /// The sequence the next row stored for a thread would take.
    ///
    /// This is the durable half of the numbering: a server that restarts
    /// numbers the rest of the conversation after what it already wrote, so a
    /// cursor from before the restart is still a position in this conversation.
    pub fn next_seq(&self, thread_id: &ThreadId) -> Result<u64, StoreError> {
        let mut statement = self
            .connection()
            .prepare("SELECT next_seq FROM thread_history WHERE thread_id = ?1")?;
        let mut rows = statement.query((thread_id.to_string(),))?;
        let counter = match rows.next()? {
            Some(row) => column_integer(row, 0)?,
            None => 1,
        };
        // `MAX(seq) + 1` as well: a header is written with every row, but the
        // counter is the authority only as long as it is never behind the rows
        // it is supposed to be ahead of.
        drop(rows);
        drop(statement);
        let mut statement = self
            .connection()
            .prepare("SELECT MAX(seq) FROM thread_history_row WHERE thread_id = ?1")?;
        let mut rows = statement.query((thread_id.to_string(),))?;
        let highest = match rows.next()? {
            Some(row) => column_optional_integer(row, 0)?
                .map(|value| u64::try_from(value).unwrap_or(0).saturating_add(1))
                .unwrap_or(1),
            None => 1,
        };
        Ok(u64::try_from(counter).unwrap_or(1).max(highest))
    }

    /// Every thread the store holds a conversation header for, with the
    /// sequence its next row would take.
    ///
    /// This is what a starting server seeds its numbering from: nothing that was
    /// written before it may be numbered again.
    pub fn next_sequences(&self) -> Result<Vec<(ThreadId, u64)>, StoreError> {
        let mut statement = self
            .connection()
            .prepare("SELECT thread_id, next_seq FROM thread_history")?;
        let mut rows = statement.query([])?;
        let mut sequences = Vec::new();
        while let Some(row) = rows.next()? {
            let raw = column_text(row, 0)?;
            let thread_id = raw
                .parse::<ThreadId>()
                .map_err(|error| StoreError::new(format!("a stored thread id {raw:?}: {error}")))?;
            let next = u64::try_from(column_integer(row, 1)?).unwrap_or(1);
            sequences.push((thread_id, next));
        }
        Ok(sequences)
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
        first_seq: u64,
        events: &[ProviderEvent],
        at_ms: u64,
    ) -> Result<u64, StoreError> {
        let transaction = self.connection().unchecked_transaction()?;
        transaction.execute(
            "INSERT INTO thread_history (thread_id) VALUES (?1)
             ON CONFLICT(thread_id) DO NOTHING",
            params![thread_id.to_string()],
        )?;
        transaction.execute(
            "DELETE FROM thread_history_row WHERE thread_id = ?1 AND source_kind = 'replayed'",
            params![thread_id.to_string()],
        )?;
        for (offset, event) in events.iter().enumerate() {
            let seq = first_seq.saturating_add(offset as u64);
            insert_row(&transaction, thread_id, seq, &RowSource::Replayed, event)?;
        }
        if let Some(last) = events.len().checked_sub(1) {
            bump_next_seq(
                &transaction,
                thread_id,
                first_seq.saturating_add(last as u64),
            )?;
        }
        transaction.execute(
            "UPDATE thread_history
                SET provider_session_id = ?2, binding_agent = ?3, binding_cwd = ?4,
                    binding_host_id = ?5, revision = revision + 1,
                    synced_at_ms = ?6, local_complete = 0, local_uncertain = 0, last_error = NULL
              WHERE thread_id = ?1",
            params![
                thread_id.to_string(),
                binding.provider_session_id.clone(),
                binding.agent.clone(),
                binding.cwd.clone(),
                binding.host_id.to_string(),
                i64::try_from(at_ms).unwrap_or(i64::MAX),
            ],
        )?;
        let mut statement =
            transaction.prepare("SELECT revision FROM thread_history WHERE thread_id = ?1")?;
        let mut rows = statement.query(params![thread_id.to_string()])?;
        let revision = match rows.next()? {
            Some(row) => column_integer(row, 0)?,
            None => 0,
        };
        drop(rows);
        drop(statement);
        transaction.commit()?;
        Ok(u64::try_from(revision).unwrap_or(0))
    }

    /// Records that the locally observed rows are a complete conversation for
    /// an agent that cannot replay session history.
    ///
    /// The caller orders this behind all accepted row writes. Requiring at
    /// least one stored row prevents an empty or unobserved thread from being
    /// declared complete by a capability fallback.
    pub fn mark_locally_complete(
        &self,
        thread_id: &ThreadId,
        binding: &CacheBinding,
    ) -> Result<(), StoreError> {
        let transaction = self.connection().unchecked_transaction()?;
        self.ensure_header(&transaction, thread_id)?;
        let uncertain: i64 = transaction.query_row(
            "SELECT local_uncertain FROM thread_history WHERE thread_id = ?1",
            params![thread_id.to_string()],
            |row| row.get(0),
        )?;
        if uncertain != 0 {
            return Err(StoreError::new(
                "the local conversation may be missing rows after an unclean stop",
            ));
        }
        let count: i64 = transaction.query_row(
            "SELECT COUNT(*) FROM thread_history_row WHERE thread_id = ?1",
            params![thread_id.to_string()],
            |row| row.get(0),
        )?;
        if count == 0 {
            return Err(StoreError::new(
                "the server has no observed conversation rows to mark complete",
            ));
        }
        transaction.execute(
            "UPDATE thread_history
                SET provider_session_id = ?2, binding_agent = ?3, binding_cwd = ?4,
                    binding_host_id = ?5, local_complete = 1, local_uncertain = 0, last_error = NULL
              WHERE thread_id = ?1",
            params![
                thread_id.to_string(),
                binding.provider_session_id.clone(),
                binding.agent.clone(),
                binding.cwd.clone(),
                binding.host_id.to_string(),
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Records that locally observed history may have a missing tail.
    ///
    /// Unlike a failed provider replay, this uncertainty must not be cleared by
    /// an unsupported-replay fallback: the missing rows cannot be reconstructed
    /// from the agent.
    pub fn mark_local_history_uncertain(
        &self,
        thread_id: &ThreadId,
        reason: &str,
    ) -> Result<(), StoreError> {
        let transaction = self.connection().unchecked_transaction()?;
        self.ensure_header(&transaction, thread_id)?;
        transaction.execute(
            "UPDATE thread_history
                SET local_uncertain = 1, last_error = ?2
              WHERE thread_id = ?1",
            params![thread_id.to_string(), reason],
        )?;
        transaction.commit()?;
        Ok(())
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
        let transaction = self.connection().unchecked_transaction()?;
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
        let transaction = self.connection().unchecked_transaction()?;
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

/// Moves a thread's next-sequence counter past `seq`, inside the caller's
/// transaction.
///
/// The counter only moves forward, which is what makes "a number is never
/// reused" hold across a rebuild: `MAX(seq)` would hand back the numbers a
/// deleted replay used to hold.
fn bump_next_seq(
    transaction: &Connection,
    thread_id: &ThreadId,
    seq: u64,
) -> Result<(), StoreError> {
    transaction.execute(
        "UPDATE thread_history SET next_seq = MAX(next_seq, ?2) WHERE thread_id = ?1",
        params![
            thread_id.to_string(),
            i64::try_from(seq.saturating_add(1)).unwrap_or(i64::MAX)
        ],
    )?;
    Ok(())
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
    fn title_fallback_uses_the_first_recorded_user_prompt() {
        let store = Store::open_in_memory().unwrap();
        let thread_id = thread();
        store
            .append_row(&thread_id, 1, &RowSource::Replayed, &message("replayed"))
            .unwrap();
        store
            .append_row(&thread_id, 2, &RowSource::Message { at_ms: 2 }, &identity())
            .unwrap();
        store
            .append_row(
                &thread_id,
                3,
                &RowSource::Message { at_ms: 3 },
                &message("  Explain the session title\nadditional context"),
            )
            .unwrap();
        store
            .append_row(
                &thread_id,
                4,
                &RowSource::Message { at_ms: 4 },
                &message("later prompt"),
            )
            .unwrap();

        assert_eq!(
            store
                .first_user_prompt_title(&thread_id)
                .unwrap()
                .as_deref(),
            Some("Explain the session title")
        );
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

        store.append_row(&thread_id, 1, &source, &event).unwrap();

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
    fn the_counter_follows_the_highest_row_written() {
        let store = Store::open_in_memory().unwrap();
        let thread_id = thread();
        for index in 1..=3 {
            store
                .append_row(
                    &thread_id,
                    index,
                    &RowSource::Message { at_ms: index },
                    &message(&index.to_string()),
                )
                .unwrap();
            assert_eq!(
                store.next_seq(&thread_id).unwrap(),
                index + 1,
                "the counter is one past the row just written"
            );
        }
        assert_eq!(store.row_count(&thread_id).unwrap(), 3);
    }

    /// A number a thread already holds is refused rather than overwritten: two
    /// rows answering to one cursor is the failure a cursor cannot detect.
    #[test]
    fn a_sequence_already_used_is_refused() {
        let store = Store::open_in_memory().unwrap();
        let thread_id = thread();
        store
            .append_row(
                &thread_id,
                1,
                &RowSource::Message { at_ms: 1 },
                &message("first"),
            )
            .unwrap();
        assert!(
            store
                .append_row(
                    &thread_id,
                    1,
                    &RowSource::Message { at_ms: 2 },
                    &message("second"),
                )
                .is_err(),
            "a sequence names one row"
        );
        assert_eq!(store.row_count(&thread_id).unwrap(), 1);
    }

    /// The numbering outlives the process: a server that restarts continues the
    /// conversation instead of numbering over what it already wrote.
    #[test]
    fn the_numbering_survives_a_reopen() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("loom.db");
        let thread_id = thread();
        let other = thread();
        {
            let store = Store::open(&path).unwrap();
            for index in 1..=3 {
                store
                    .append_row(
                        &thread_id,
                        index,
                        &RowSource::Message { at_ms: index },
                        &message(&index.to_string()),
                    )
                    .unwrap();
            }
            store
                .append_row(
                    &other,
                    7,
                    &RowSource::Message { at_ms: 7 },
                    &message("elsewhere"),
                )
                .unwrap();
        }

        let store = Store::open(&path).unwrap();
        assert_eq!(store.next_seq(&thread_id).unwrap(), 4);
        let seqs = store.next_sequences().unwrap();
        assert!(
            seqs.contains(&(thread_id.clone(), 4)) && seqs.contains(&(other, 8)),
            "every thread's counter is readable: {seqs:?}"
        );
        store
            .append_row(
                &thread_id,
                4,
                &RowSource::Message { at_ms: 4 },
                &message("after the restart"),
            )
            .unwrap();
        assert_eq!(store.row_count(&thread_id).unwrap(), 4);
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
                1,
                &RowSource::Message { at_ms: 10 },
                &message("a prompt"),
            )
            .unwrap();
        store
            .replace_replayed(
                &thread_id,
                &binding,
                2,
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
            .replace_replayed(&thread_id, &binding, 4, &[identity()], 30)
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

    /// A provider replay becomes the source again if a formerly resume-only
    /// agent later supports transcript loading.
    #[test]
    fn a_provider_replay_clears_local_history_authority() {
        let store = Store::open_in_memory().unwrap();
        let thread_id = thread();
        let binding = binding("dsh");
        store
            .append_row(
                &thread_id,
                1,
                &RowSource::Message { at_ms: 10 },
                &message("local prompt"),
            )
            .unwrap();
        store
            .mark_locally_complete(&thread_id, &binding)
            .expect("the stored prompt anchors a local conversation");
        assert!(store.history(&thread_id).unwrap().unwrap().local_complete);

        store
            .replace_replayed(&thread_id, &binding, 2, &[identity()], 20)
            .unwrap();
        assert!(!store.history(&thread_id).unwrap().unwrap().local_complete);
    }

    /// A live row appended to the current conversation keeps the revision: the
    /// client's cursor is still a position in the same numbering.
    #[test]
    fn an_appended_row_does_not_move_the_revision() {
        let store = Store::open_in_memory().unwrap();
        let thread_id = thread();
        let binding = binding("pi");
        store
            .replace_replayed(&thread_id, &binding, 1, &[identity()], 10)
            .unwrap();
        store
            .append_row(
                &thread_id,
                2,
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
            .replace_replayed(&thread_id, &binding, 1, &[identity()], 10)
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
                1,
                &RowSource::Message { at_ms: 1 },
                &message("mine"),
            )
            .unwrap();
        store
            .append_row(
                &other,
                1,
                &RowSource::Message { at_ms: 1 },
                &message("theirs"),
            )
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
