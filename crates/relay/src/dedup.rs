//! Idempotence primitive.
//!
//! Replay is what makes the relay reliable, and replay is also what makes
//! duplicate delivery normal: an event can arrive on the local fast path and
//! again from the log, or be replayed after a reconnect. Every consumer
//! therefore needs to recognise an [`EventId`] it has already handled.
//!
//! [`SeenSet`] is a bounded FIFO of recently seen ids with O(1) membership.
//! The bound is what keeps this from becoming a leak on a long-lived
//! connection; it is sized so that a duplicate arriving within a few hundred
//! events is still caught.

use std::collections::{HashSet, VecDeque};

use crate::event_id::EventId;

/// Default number of recent event ids remembered per consumer.
pub const DEFAULT_DEDUP_CAPACITY: usize = 128;

/// A bounded set of recently seen event ids.
#[derive(Debug, Clone)]
pub struct SeenSet {
    capacity: usize,
    order: VecDeque<EventId>,
    seen: HashSet<EventId>,
}

impl SeenSet {
    /// Creates a set holding at most `capacity` ids (minimum 1).
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            order: VecDeque::new(),
            seen: HashSet::new(),
        }
    }

    /// Records `id`.
    ///
    /// Returns `true` the first time an id is seen — the caller should deliver
    /// — and `false` for a duplicate, which the caller must drop.
    pub fn insert(&mut self, id: EventId) -> bool {
        if self.seen.contains(&id) {
            return false;
        }
        self.seen.insert(id);
        self.order.push_back(id);
        while self.order.len() > self.capacity {
            if let Some(evicted) = self.order.pop_front() {
                self.seen.remove(&evicted);
            }
        }
        true
    }

    /// Whether `id` is currently remembered.
    pub fn contains(&self, id: &EventId) -> bool {
        self.seen.contains(id)
    }

    /// Number of remembered ids.
    pub fn len(&self) -> usize {
        self.seen.len()
    }

    /// Whether nothing is remembered.
    pub fn is_empty(&self) -> bool {
        self.seen.is_empty()
    }

    /// Configured capacity.
    pub fn capacity(&self) -> usize {
        self.capacity
    }
}

impl Default for SeenSet {
    fn default() -> Self {
        SeenSet::new(DEFAULT_DEDUP_CAPACITY)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_occurrence_is_delivered_and_repeats_are_dropped() {
        let mut set = SeenSet::new(8);
        let id = EventId::new();
        assert!(set.insert(id));
        assert!(!set.insert(id));
        assert!(!set.insert(id));
        assert_eq!(set.len(), 1);
    }

    #[test]
    fn distinct_ids_are_each_delivered_once() {
        let mut set = SeenSet::new(1_000);
        let ids: Vec<EventId> = (0..500).map(|_| EventId::new()).collect();
        for id in &ids {
            assert!(set.insert(*id));
        }
        for id in &ids {
            assert!(!set.insert(*id));
        }
        assert_eq!(set.len(), 500);
    }

    #[test]
    fn eviction_forgets_the_oldest_first() {
        let mut set = SeenSet::new(2);
        let (a, b, c) = (EventId::new(), EventId::new(), EventId::new());

        assert!(set.insert(a));
        assert!(set.insert(b));
        assert!(set.insert(c));

        assert_eq!(set.len(), 2);
        assert!(!set.contains(&a), "oldest must be evicted");
        assert!(set.contains(&b));
        assert!(set.contains(&c));
        // A forgotten id is treated as new again.
        assert!(set.insert(a));
    }

    #[test]
    fn never_grows_beyond_capacity() {
        let mut set = SeenSet::new(16);
        for _ in 0..1_000 {
            set.insert(EventId::new());
        }
        assert_eq!(set.len(), 16);
    }

    #[test]
    fn capacity_is_at_least_one() {
        let mut set = SeenSet::new(0);
        assert_eq!(set.capacity(), 1);
        assert!(set.insert(EventId::new()));
        assert_eq!(set.len(), 1);
    }
}
