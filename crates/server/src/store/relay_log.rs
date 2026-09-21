//! The relay's frames, as rows in the store.
//!
//! The frames are what a client resumes from, what a worker's missed frames are
//! caught up with, and what recovery replays after the entity view's watermark.
//! They used to be an append-only file per shard; they are rows now, in the same
//! database as the conversations and the entity view, so the durable state is
//! one file and a transaction can span a frame and the state it implies.
//!
//! Two rules, both inherited from the file backends they replace:
//!
//! * **`event_id` is the identity and the order.** It is a fixed-width Crockford
//!   base32 of a `u128`, so its text order is the numeric order the in-memory
//!   backends filter by. A reader asks for what is *newer than* its cursor, which
//!   is why a same-millisecond burst larger than one page cannot stall it: the
//!   cursor is part of the read, not a filter the caller applies afterwards.
//! * **A shard is bounded, oldest first.** Appending past the bound drops the
//!   oldest frames in the same transaction, so a burst cannot grow the store
//!   without bound and a reader never sees a hole in the middle.

use bytes::Bytes;
use rusqlite::params;

use super::{column_blob, column_integer, column_optional_text, column_text, Store, StoreError};
use loom_relay::backend::LogRecord;
use loom_relay::{EventId, Scope};

impl Store {
    /// Appends one frame to a shard, dropping the oldest if the shard is full.
    ///
    /// One transaction with the trim, so a reader either sees the shard within
    /// its bound or the frame it is about to see; there is no moment where the
    /// shard is over the bound and no moment where the frame is missing.
    pub fn append_relay_event(
        &self,
        shard: u8,
        record: &LogRecord,
        max_len: usize,
    ) -> Result<(), StoreError> {
        let transaction = self.connection().unchecked_transaction()?;
        transaction.execute(
            "INSERT INTO relay_event
                (event_id, shard, scope_kind, scope_id, payload, created_at_ms, origin)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(event_id) DO NOTHING",
            params![
                record.event_id.to_string(),
                i64::from(shard),
                record.scope.kind(),
                record.scope.id(),
                record.payload.to_vec(),
                i64::try_from(record.created_at_ms).unwrap_or(i64::MAX),
                record.origin.clone(),
            ],
        )?;
        // Keep the newest `max_len` frames. The subquery answers with the oldest
        // frame that must go, and nothing at all when the shard is within its
        // bound — a comparison against nothing deletes nothing.
        transaction.execute(
            "DELETE FROM relay_event
              WHERE shard = ?1 AND event_id <= (
                  SELECT event_id FROM relay_event
                   WHERE shard = ?1 ORDER BY event_id DESC LIMIT 1 OFFSET ?2
              )",
            params![
                i64::from(shard),
                i64::try_from(max_len.max(1)).unwrap_or(i64::MAX)
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// The frames of a shard newer than `after`, in id order, up to `limit`.
    pub fn read_relay_events(
        &self,
        shard: u8,
        after: Option<EventId>,
        limit: usize,
    ) -> Result<Vec<LogRecord>, StoreError> {
        let mut statement = self.connection().prepare(
            "SELECT event_id, scope_kind, scope_id, payload, created_at_ms, origin
               FROM relay_event
              WHERE shard = ?1 AND event_id > ?2
              ORDER BY event_id
              LIMIT ?3",
        )?;
        let mut rows = statement.query(params![
            i64::from(shard),
            after.map(|id| id.to_string()).unwrap_or_default(),
            i64::try_from(limit).unwrap_or(i64::MAX),
        ])?;
        let mut records = Vec::new();
        while let Some(row) = rows.next()? {
            let event_id = column_text(row, 0)?;
            let event_id = event_id.parse::<EventId>().map_err(|error| {
                StoreError::new(format!("a stored frame id {event_id:?}: {error}"))
            })?;
            let kind = column_text(row, 1)?;
            let id = column_optional_text(row, 2)?.unwrap_or_default();
            let scope = Scope::from_kind_id(&kind, id).ok_or_else(|| {
                StoreError::new(format!(
                    "a stored frame names an unknown scope kind {kind:?}"
                ))
            })?;
            let payload = column_blob(row, 3)?;
            let created_at_ms = column_integer(row, 4)?;
            let origin = column_text(row, 5)?;
            records.push(LogRecord {
                event_id,
                scope,
                payload: Bytes::from(payload),
                created_at_ms: u64::try_from(created_at_ms).unwrap_or(0),
                origin,
            });
        }
        Ok(records)
    }

    /// Drops a shard's frames older than `before_ms`, returning how many went.
    pub fn trim_relay_events(&self, shard: u8, before_ms: u64) -> Result<u64, StoreError> {
        let removed = self.connection().execute(
            "DELETE FROM relay_event WHERE shard = ?1 AND created_at_ms < ?2",
            params![
                i64::from(shard),
                i64::try_from(before_ms).unwrap_or(i64::MAX)
            ],
        )?;
        Ok(u64::try_from(removed).unwrap_or(u64::MAX))
    }

    /// How many frames a shard holds.
    pub fn relay_event_count(&self, shard: u8) -> Result<usize, StoreError> {
        let mut statement = self
            .connection()
            .prepare("SELECT COUNT(*) FROM relay_event WHERE shard = ?1")?;
        let mut rows = statement.query(params![i64::from(shard)])?;
        let count = match rows.next()? {
            Some(row) => column_integer(row, 0)?,
            None => 0,
        };
        Ok(usize::try_from(count).unwrap_or(0))
    }

    /// Every shard's frame count, for diagnostics.
    pub fn relay_event_total(&self) -> Result<usize, StoreError> {
        let mut statement = self
            .connection()
            .prepare("SELECT COUNT(*) FROM relay_event")?;
        let mut rows = statement.query([])?;
        let count = match rows.next()? {
            Some(row) => column_integer(row, 0)?,
            None => 0,
        };
        Ok(usize::try_from(count).unwrap_or(0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(scope: Scope, at_ms: u64) -> LogRecord {
        LogRecord {
            event_id: EventId::new(),
            scope,
            payload: Bytes::from_static(b"{\"hello\":true}"),
            created_at_ms: at_ms,
            origin: "node-1".to_owned(),
        }
    }

    #[test]
    fn a_frame_comes_back_exactly_as_it_went_in() {
        let store = Store::open_in_memory().unwrap();
        let written = record(Scope::Thread("thr_1".to_owned()), 1_700_000_000_000);
        store.append_relay_event(3, &written, 100).unwrap();

        assert_eq!(store.read_relay_events(3, None, 10).unwrap(), vec![written]);
        assert_eq!(store.relay_event_count(3).unwrap(), 1);
        assert!(store.read_relay_events(4, None, 10).unwrap().is_empty());
    }

    /// The property the file backends were built around: a reader makes
    /// progress because the cursor is part of the read, even when a burst is
    /// larger than one page and every frame shares a millisecond.
    #[test]
    fn a_burst_larger_than_a_page_still_makes_progress() {
        let store = Store::open_in_memory().unwrap();
        for _ in 0..5 {
            let frame = record(Scope::Thread("thr_1".to_owned()), 1_700_000_000_000);
            store.append_relay_event(1, &frame, 100).unwrap();
        }
        let mut cursor = None;
        let mut seen = 0;
        loop {
            let page = store.read_relay_events(1, cursor, 2).unwrap();
            if page.is_empty() {
                break;
            }
            seen += page.len();
            cursor = Some(page.last().unwrap().event_id);
        }
        assert_eq!(seen, 5, "every frame was read exactly once");
    }

    /// A shard is bounded, and what goes when it is full is the oldest frame.
    #[test]
    fn a_full_shard_drops_its_oldest_frame() {
        let store = Store::open_in_memory().unwrap();
        let mut ids = Vec::new();
        for index in 0..4 {
            let frame = record(Scope::Thread("thr_1".to_owned()), 10 + index);
            ids.push(frame.event_id);
            store.append_relay_event(1, &frame, 3).unwrap();
        }
        assert_eq!(store.relay_event_count(1).unwrap(), 3);
        let held: Vec<EventId> = store
            .read_relay_events(1, None, 10)
            .unwrap()
            .into_iter()
            .map(|frame| frame.event_id)
            .collect();
        assert_eq!(
            held,
            ids[1..].to_vec(),
            "the oldest frame went and the rest stayed in order"
        );
    }

    #[test]
    fn trimming_by_age_removes_only_what_is_older() {
        let store = Store::open_in_memory().unwrap();
        for at_ms in [10, 20, 30] {
            let frame = record(Scope::Thread("thr_1".to_owned()), at_ms);
            store.append_relay_event(1, &frame, 100).unwrap();
        }
        assert_eq!(store.trim_relay_events(1, 20).unwrap(), 1);
        let held = store.read_relay_events(1, None, 10).unwrap();
        assert_eq!(
            held.iter()
                .map(|frame| frame.created_at_ms)
                .collect::<Vec<_>>(),
            vec![20, 30]
        );
    }

    /// Every scope kind round-trips, the global one included: a frame that came
    /// back as another scope would be delivered to the wrong readers.
    #[test]
    fn every_scope_kind_round_trips() {
        let store = Store::open_in_memory().unwrap();
        let scopes = [
            Scope::Global,
            Scope::Project("prj_1".to_owned()),
            Scope::Thread("thr_1".to_owned()),
            Scope::Host("host_1".to_owned()),
            Scope::Client("cl_1".to_owned()),
            Scope::User("usr_1".to_owned()),
        ];
        for scope in &scopes {
            let frame = record(scope.clone(), 10);
            store.append_relay_event(1, &frame, 100).unwrap();
        }
        let mut held: Vec<Scope> = store
            .read_relay_events(1, None, 10)
            .unwrap()
            .into_iter()
            .map(|frame| frame.scope)
            .collect();
        held.sort_by_key(|scope| format!("{}:{}", scope.kind(), scope.id()));
        let mut expected = scopes.to_vec();
        expected.sort_by_key(|scope| format!("{}:{}", scope.kind(), scope.id()));
        assert_eq!(held, expected);
    }
}
