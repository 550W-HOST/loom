//! Who hands out a conversation's sequence numbers.
//!
//! A row's sequence has to name the same row in two places at once: the durable
//! conversation, and the displayable one the server shows *before* the write
//! lands. The store cannot be the one to assign it — that would put a disk write
//! on the path of every streamed frame — and it cannot be derived from position,
//! because that is the bug this store exists to remove. So the publisher
//! reserves it here, and both the durable row and the displayable row carry the
//! number the publisher was given.
//!
//! Two rules keep it honest:
//!
//! * **Seeded from the store, never from one.** A starting server reads each
//!   thread's next sequence out of the store, so a restarted process continues
//!   the conversation instead of numbering over what it already wrote.
//! * **Only forward.** A rebuild takes numbers past everything already handed
//!   out, so a cursor from before a rebuild can never be read as a position in
//!   the rebuilt conversation — the revision moves with it, and the two together
//!   are what a client's cursor names.

use std::collections::HashMap;
use std::sync::Mutex;

use loom_domain::ThreadId;

use super::{Store, StoreError};

/// The numbering every thread's conversation is written in.
pub struct SeqAllocator {
    next: Mutex<HashMap<ThreadId, u64>>,
}

impl SeqAllocator {
    /// Reads the numbering out of the store.
    ///
    /// A thread the store has never written for is absent, which is the same
    /// answer as starting at one: it has no numbers yet.
    pub fn seeded_from(store: &Store) -> Result<Self, StoreError> {
        Ok(Self {
            next: Mutex::new(store.next_sequences()?.into_iter().collect()),
        })
    }

    /// An allocator with nothing to continue, for tests and for a thread that
    /// is about to be created.
    pub fn empty() -> Self {
        Self {
            next: Mutex::new(HashMap::new()),
        }
    }

    /// Reserves `count` numbers for a thread, returning the first.
    ///
    /// The numbers are the caller's now: nothing else will reserve them, and a
    /// gap left by a caller that does not use them is intentional — a position
    /// in a conversation is never handed out twice.
    pub fn reserve(&self, thread_id: &ThreadId, count: usize) -> u64 {
        let mut next = self
            .next
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let first = next.entry(thread_id.clone()).or_insert(1);
        let reserved = *first;
        *first = first.saturating_add(count as u64);
        reserved
    }

    /// The next number a thread would reserve, without taking one.
    ///
    /// A load records this when it starts and checks it again when the replay
    /// arrives: a row published in between means the replay is not the whole
    /// conversation any more.
    pub fn position(&self, thread_id: &ThreadId) -> u64 {
        self.next
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(thread_id)
            .copied()
            .unwrap_or(1)
    }

    /// Forgets a thread's numbering, which only deleting the thread may do: its
    /// rows are gone with it, so there is nothing left to number after.
    pub fn forget(&self, thread_id: &ThreadId) {
        self.next
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(thread_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history_cache::RowSource;
    use loom_domain::{ProviderEvent, ThreadEventItem, UserContent};

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

    #[test]
    fn numbers_are_reserved_in_order_and_never_repeated() {
        let allocator = SeqAllocator::empty();
        let thread = ThreadId::mint();
        assert_eq!(allocator.reserve(&thread, 1), 1);
        assert_eq!(allocator.reserve(&thread, 3), 2);
        assert_eq!(allocator.position(&thread), 5);
        assert_eq!(allocator.reserve(&thread, 1), 5);
        // Another thread has its own numbering.
        assert_eq!(allocator.reserve(&ThreadId::mint(), 1), 1);
    }

    /// The seed is what makes a restart continue a conversation rather than
    /// number over it.
    #[test]
    fn seeding_continues_the_numbering_the_store_holds() {
        let store = Store::open_in_memory().unwrap();
        let thread = ThreadId::mint();
        store
            .append_row(
                &thread,
                1,
                &RowSource::Message { at_ms: 1 },
                &message("before"),
            )
            .unwrap();

        let allocator = SeqAllocator::seeded_from(&store).unwrap();
        assert_eq!(allocator.position(&thread), 2);
        assert_eq!(allocator.reserve(&thread, 1), 2);
    }

    #[test]
    fn forgetting_a_thread_starts_it_from_one_again() {
        let allocator = SeqAllocator::empty();
        let thread = ThreadId::mint();
        allocator.reserve(&thread, 4);
        allocator.forget(&thread);
        assert_eq!(allocator.reserve(&thread, 1), 1);
    }
}
