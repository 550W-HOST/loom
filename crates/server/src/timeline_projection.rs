//! The projected timeline, remembered per conversation.
//!
//! A conversation's rows are projected from *every* frame it holds: answers
//! fold from many deltas, calls from several frames, and the result is sorted.
//! That work is proportional to the whole conversation, and a conversation can
//! be tens of thousands of frames even though its timeline is a few hundred
//! rows. A page request only ever wants a slice of those rows, so doing the
//! whole projection per request makes paging cost the same as the whole
//! conversation, over and over: reading the frames dominates, and the store
//! lock is held for the duration.
//!
//! This remembers the projection for the conversations that are being read.
//! It is valid while the conversation's `(revision, last sequence)` is
//! unchanged — every append moves the last sequence and a rebuilt baseline
//! moves the revision, so nothing that changes a row's projection leaves the
//! key alone. A hit turns a page request into a slice of memory.
//!
//! It is a cache, not a source: an evicted or missed thread costs one read of
//! its conversation, exactly what every request used to cost.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use loom_domain::ThreadId;
use serde_json::Value;

/// A conversation projected into timeline rows, oldest first.
pub(crate) struct TimelineProjection {
    /// The projected rows.
    pub rows: Vec<Value>,
    /// The newest model fallback a provider reported, if any.
    pub model_fallback: Option<Value>,
    /// The last input's sequence, which is the cursor a client resumes from.
    pub max_seq: u64,
    /// The newest plan snapshot among the rows, if any.
    ///
    /// Carried without the thread's status on purpose: the plan is a fact about
    /// the conversation, and whether it is shown as a to-do card depends on the
    /// thread being in flight, which the reader decides.
    pub latest_plan_todos: Value,
}

impl TimelineProjection {
    /// A projection of an empty conversation.
    pub(crate) fn empty() -> Self {
        Self {
            rows: Vec::new(),
            model_fallback: None,
            max_seq: 0,
            latest_plan_todos: Value::Null,
        }
    }
}

/// What a cached projection is valid for.
///
/// The revision names the numbering a rebuilt baseline moved; the last
/// sequence moves with every append. A row's projection cannot change without
/// one of the two moving, so together they are the whole validity condition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ProjectionKey {
    /// The conversation revision the rows were projected from.
    pub revision: u64,
    /// The highest sequence the conversation held.
    pub last_seq: u64,
}

struct Entry {
    key: ProjectionKey,
    projection: Arc<TimelineProjection>,
    last_used: u64,
}

struct Inner {
    entries: HashMap<ThreadId, Entry>,
    next_tick: u64,
    total_rows: usize,
    /// How many reads were answered from memory. Tests use it to tell a hit
    /// from a miss without a counter in the production path.
    #[cfg(test)]
    hits: u64,
}

/// A bounded, least-recently-used set of projected conversations.
pub(crate) struct TimelineProjectionCache {
    inner: Mutex<Inner>,
    max_entries: usize,
    max_rows_per_entry: usize,
    max_total_rows: usize,
}

impl TimelineProjectionCache {
    /// Creates a cache holding at most `max_entries` conversations, none with
    /// more than `max_rows_per_entry` rows and `max_total_rows` rows in all.
    ///
    /// A conversation whose projection is larger than one entry can hold is
    /// simply not cached: a projection of every row of a giant thread is
    /// exactly the thing the bounds exist to keep out of memory.
    pub(crate) fn new(
        max_entries: usize,
        max_rows_per_entry: usize,
        max_total_rows: usize,
    ) -> Self {
        Self {
            inner: Mutex::new(Inner {
                entries: HashMap::new(),
                next_tick: 0,
                total_rows: 0,
                #[cfg(test)]
                hits: 0,
            }),
            max_entries: max_entries.max(1),
            max_rows_per_entry: max_rows_per_entry.max(1),
            max_total_rows: max_total_rows.max(1),
        }
    }

    /// The projection for a conversation, when it is still valid for `key`.
    pub(crate) fn get(
        &self,
        thread_id: &ThreadId,
        key: ProjectionKey,
    ) -> Option<Arc<TimelineProjection>> {
        let mut inner = self.lock();
        let entry = inner.entries.get(thread_id)?;
        if entry.key != key {
            return None;
        }
        #[cfg(test)]
        {
            inner.hits += 1;
        }
        let tick = inner.next_tick;
        inner.next_tick += 1;
        let entry = inner.entries.get_mut(thread_id)?;
        entry.last_used = tick;
        Some(Arc::clone(&entry.projection))
    }

    /// Remembers a conversation's projection under the key it was built from.
    pub(crate) fn put(
        &self,
        thread_id: &ThreadId,
        key: ProjectionKey,
        projection: Arc<TimelineProjection>,
    ) {
        let rows = projection.rows.len();
        let mut inner = self.lock();
        if rows > self.max_rows_per_entry {
            inner.remove(thread_id);
            return;
        }
        let tick = inner.next_tick;
        inner.next_tick += 1;
        if let Some(previous) = inner.entries.get(thread_id) {
            inner.total_rows = inner
                .total_rows
                .saturating_sub(previous.projection.rows.len());
        }
        inner.total_rows = inner.total_rows.saturating_add(rows);
        inner.entries.insert(
            thread_id.clone(),
            Entry {
                key,
                projection,
                last_used: tick,
            },
        );
        while inner.entries.len() > self.max_entries || inner.total_rows > self.max_total_rows {
            let Some(oldest) = inner
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(id, _)| id.clone())
            else {
                break;
            };
            inner.remove(&oldest);
        }
    }

    /// Forgets a conversation's projection.
    pub(crate) fn remove(&self, thread_id: &ThreadId) {
        self.lock().remove(thread_id);
    }

    /// How many conversations are cached.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.lock().entries.len()
    }

    /// How many reads were answered from memory.
    #[cfg(test)]
    pub(crate) fn hits(&self) -> u64 {
        self.lock().hits
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Inner {
    fn remove(&mut self, thread_id: &ThreadId) {
        if let Some(entry) = self.entries.remove(thread_id) {
            self.total_rows = self.total_rows.saturating_sub(entry.projection.rows.len());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn projection(rows: usize) -> Arc<TimelineProjection> {
        Arc::new(TimelineProjection {
            rows: (0..rows).map(|index| Value::from(index as u64)).collect(),
            model_fallback: None,
            max_seq: rows as u64,
            latest_plan_todos: Value::Null,
        })
    }

    #[test]
    fn a_key_change_is_a_miss() {
        let cache = TimelineProjectionCache::new(4, 10, 40);
        let thread = ThreadId::mint();
        let built = projection(3);
        cache.put(
            &thread,
            ProjectionKey {
                revision: 1,
                last_seq: 9,
            },
            Arc::clone(&built),
        );

        assert!(cache
            .get(
                &thread,
                ProjectionKey {
                    revision: 1,
                    last_seq: 10,
                }
            )
            .is_none());
        assert!(cache
            .get(
                &thread,
                ProjectionKey {
                    revision: 2,
                    last_seq: 9,
                }
            )
            .is_none());
        assert!(Arc::ptr_eq(
            &cache
                .get(
                    &thread,
                    ProjectionKey {
                        revision: 1,
                        last_seq: 9,
                    }
                )
                .expect("the same key still hits"),
            &built
        ));
    }

    #[test]
    fn a_projection_larger_than_one_entry_is_not_cached() {
        let cache = TimelineProjectionCache::new(4, 2, 40);
        let thread = ThreadId::mint();
        cache.put(
            &thread,
            ProjectionKey {
                revision: 1,
                last_seq: 3,
            },
            projection(3),
        );

        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn the_least_recently_used_conversation_is_evicted() {
        let cache = TimelineProjectionCache::new(2, 10, 40);
        let first = ThreadId::mint();
        let second = ThreadId::mint();
        let third = ThreadId::mint();
        for thread in [&first, &second] {
            cache.put(
                thread,
                ProjectionKey {
                    revision: 1,
                    last_seq: 1,
                },
                projection(1),
            );
        }
        // Touch the first so the second is now the least recently used.
        let _ = cache.get(
            &first,
            ProjectionKey {
                revision: 1,
                last_seq: 1,
            },
        );
        cache.put(
            &third,
            ProjectionKey {
                revision: 1,
                last_seq: 1,
            },
            projection(1),
        );

        assert!(cache
            .get(
                &first,
                ProjectionKey {
                    revision: 1,
                    last_seq: 1,
                }
            )
            .is_some());
        assert!(cache
            .get(
                &second,
                ProjectionKey {
                    revision: 1,
                    last_seq: 1,
                }
            )
            .is_none());
        assert!(cache
            .get(
                &third,
                ProjectionKey {
                    revision: 1,
                    last_seq: 1,
                }
            )
            .is_some());
    }

    #[test]
    fn the_total_row_bound_evicts_the_oldest() {
        let cache = TimelineProjectionCache::new(10, 10, 3);
        let first = ThreadId::mint();
        let second = ThreadId::mint();
        cache.put(
            &first,
            ProjectionKey {
                revision: 1,
                last_seq: 2,
            },
            projection(2),
        );
        cache.put(
            &second,
            ProjectionKey {
                revision: 1,
                last_seq: 2,
            },
            projection(2),
        );

        assert!(cache
            .get(
                &first,
                ProjectionKey {
                    revision: 1,
                    last_seq: 2,
                }
            )
            .is_none());
        assert!(cache
            .get(
                &second,
                ProjectionKey {
                    revision: 1,
                    last_seq: 2,
                }
            )
            .is_some());
    }
}
