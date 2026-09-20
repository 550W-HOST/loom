//! The durable side of the publish seam: display first, store behind it.
//!
//! A conversation is shown from memory, so nothing a person waits on may block
//! on a disk. The store is therefore written from **its own thread**, fed by a
//! **bounded** queue: a publish enqueues a row and returns, and the thread that
//! owns the connection turns those rows into transactions.
//!
//! Two failure modes are handled honestly rather than papered over:
//!
//! * **The queue is full.** The row is refused and the thread is marked unsaved
//!   with that reason. Growing without bound would trade a slow disk for the
//!   server's memory, and silently dropping the row would make the stored
//!   conversation quietly miss a turn.
//! * **A write fails.** The same: the thread is marked unsaved with the error.
//!   The mark is *sticky*, because a row that never reached the disk is a hole
//!   in the middle of the conversation, and a later row succeeding does not fill
//!   it. Only a rebuilt conversation — one that replaces what is stored whole —
//!   clears it.
//!
//! What this deliberately does not do: retry forever (a broken disk gets worse,
//! not better), block the publisher, or claim the stored conversation is
//! complete when it is not.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use loom_domain::{ProviderEvent, ThreadId};

use super::Store;
use crate::history_cache::RowSource;

/// How long the writer waits for work before checking whether it should stop.
///
/// It only ever notices a stop when the queue is empty, so this is the longest
/// a shutdown waits for an idle writer, not a latency on any row.
const IDLE: Duration = Duration::from_millis(50);

/// One row waiting to be stored.
#[derive(Clone, Debug)]
struct StoreWrite {
    thread_id: ThreadId,
    /// The number its publisher reserved, so the stored row and the row already
    /// on screen name the same position.
    seq: u64,
    source: RowSource,
    event: ProviderEvent,
}

/// What the writer shares with the publish path.
#[derive(Debug, Default)]
struct Shared {
    /// Threads whose stored conversation is known to be incomplete, and why.
    ///
    /// Sticky by design: a later row that succeeds does not fill a hole an
    /// earlier one left.
    unsaved: Mutex<HashMap<ThreadId, String>>,
    /// Rows written successfully, for tests and for noticing a stalled writer.
    written: AtomicU64,
    /// Set when the server is stopping: no new row is accepted, and the writer
    /// finishes what it already has.
    stopping: AtomicBool,
}

impl Shared {
    fn mark_unsaved(&self, thread_id: &ThreadId, reason: impl Into<String>) {
        self.unsaved
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entry(thread_id.clone())
            .or_insert_with(|| reason.into());
    }
}

/// The conversation's writer: a thread, a queue, and what it could not store.
#[derive(Debug)]
pub struct StoreWriter {
    sender: SyncSender<StoreWrite>,
    shared: Arc<Shared>,
    handle: Mutex<Option<JoinHandle<()>>>,
}

impl StoreWriter {
    /// Starts a writer over `store`, accepting at most `capacity` pending rows.
    pub fn spawn(store: Arc<Mutex<Store>>, capacity: usize) -> Self {
        let (sender, receiver) = std::sync::mpsc::sync_channel(capacity.max(1));
        let shared = Arc::new(Shared::default());
        let writer_shared = Arc::clone(&shared);
        let handle = std::thread::Builder::new()
            .name("loom-store-writer".to_owned())
            .spawn(move || run(store, receiver, writer_shared))
            .expect("the store writer thread could not be started");
        Self {
            sender,
            shared,
            handle: Mutex::new(Some(handle)),
        }
    }

    /// Queues one row, without blocking the publisher.
    ///
    /// Returns whether the row was accepted. A refusal is not silent: the thread
    /// is marked unsaved with the reason, so the status a reader sees says the
    /// stored conversation is incomplete.
    pub fn enqueue(
        &self,
        thread_id: &ThreadId,
        seq: u64,
        source: RowSource,
        event: ProviderEvent,
    ) -> bool {
        if self.shared.stopping.load(Ordering::SeqCst) {
            self.shared
                .mark_unsaved(thread_id, "the server is stopping; the row was not stored");
            return false;
        }
        let write = StoreWrite {
            thread_id: thread_id.clone(),
            seq,
            source,
            event,
        };
        match self.sender.try_send(write) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => {
                self.shared.mark_unsaved(
                    thread_id,
                    "the store is behind and the row was refused rather than queued without bound",
                );
                false
            }
            Err(TrySendError::Disconnected(_)) => {
                self.shared
                    .mark_unsaved(thread_id, "the store writer is not running");
                false
            }
        }
    }

    /// Why this thread's stored conversation is known to be incomplete.
    pub fn unsaved(&self, thread_id: &ThreadId) -> Option<String> {
        self.shared
            .unsaved
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(thread_id)
            .cloned()
    }

    /// How many threads are known to have an incomplete stored conversation.
    pub fn unsaved_count(&self) -> usize {
        self.shared
            .unsaved
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len()
    }

    /// Rows written successfully so far.
    pub fn written(&self) -> u64 {
        self.shared.written.load(Ordering::SeqCst)
    }

    /// Forgets that a thread was unsaved.
    ///
    /// Only a rebuilt conversation may call this: the holes a failed write left
    /// are filled by replacing what is stored, not by a later row succeeding.
    pub fn clear_unsaved(&self, thread_id: &ThreadId) {
        self.shared
            .unsaved
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(thread_id);
    }

    /// Stops accepting rows, drains the queue, and waits for the writer.
    ///
    /// Returns why the writer could not finish when it could not — a stop that
    /// did not store what it accepted is a durability failure, and the caller is
    /// already on its way out with an exit code to choose.
    pub fn flush(&self) -> Result<(), String> {
        self.shared.stopping.store(true, Ordering::SeqCst);
        let handle = self
            .handle
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        let Some(handle) = handle else {
            return Ok(());
        };
        handle
            .join()
            .map_err(|_| "the store writer thread panicked".to_owned())
    }

    /// Waits until `count` rows have been written, or `timeout` passes.
    ///
    /// For tests: the publish path returns before the disk does, so a test that
    /// asserts what is stored has to wait for the writer to catch up.
    #[cfg(test)]
    pub(crate) fn wait_for_writes(&self, count: u64, timeout: Duration) -> bool {
        use std::time::Instant;

        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.written() >= count {
                return true;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        self.written() >= count
    }
}

/// The writer thread: drain the queue, one transaction per row.
fn run(store: Arc<Mutex<Store>>, receiver: Receiver<StoreWrite>, shared: Arc<Shared>) {
    loop {
        let write = match receiver.recv_timeout(IDLE) {
            Ok(write) => write,
            Err(RecvTimeoutError::Timeout) => {
                // Only an empty queue lets the writer notice it should stop, so
                // nothing accepted is ever dropped on the way out.
                if shared.stopping.load(Ordering::SeqCst) {
                    return;
                }
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => return,
        };
        write_one(&store, &shared, write);
    }
}

fn write_one(store: &Arc<Mutex<Store>>, shared: &Shared, write: StoreWrite) {
    let store = store
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    match store.append_row(&write.thread_id, write.seq, &write.source, &write.event) {
        Ok(_) => {
            shared.written.fetch_add(1, Ordering::SeqCst);
        }
        Err(error) => {
            // The row is lost, so the thread stays marked until a rebuild
            // replaces what is stored. Reporting it is the whole point: a
            // conversation that is quietly missing a turn is worse than one that
            // says it is incomplete.
            shared.mark_unsaved(
                &write.thread_id,
                format!("the store could not write a row: {error}"),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use loom_domain::{ThreadEventItem, UserContent};
    use std::time::Instant;

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

    fn source(at_ms: u64) -> RowSource {
        RowSource::Message { at_ms }
    }

    /// A publisher never waits for a disk: a full queue refuses the row and says
    /// so, instead of growing without bound or blocking the request.
    #[test]
    fn a_full_queue_refuses_the_row_and_marks_the_thread() {
        let (sender, _receiver) = std::sync::mpsc::sync_channel(1);
        let writer = StoreWriter {
            sender,
            shared: Arc::new(Shared::default()),
            handle: Mutex::new(None),
        };
        let thread_id = ThreadId::mint();

        assert!(writer.enqueue(&thread_id, 1, source(1), message("first")));
        assert!(
            !writer.enqueue(&thread_id, 2, source(2), message("second")),
            "a full queue refuses rather than blocking"
        );
        let reason = writer.unsaved(&thread_id).expect("the thread is marked");
        assert!(reason.contains("behind"), "{reason}");
    }

    /// A store that cannot write marks the thread unsaved and *keeps* it marked:
    /// a hole in the middle of a conversation is not filled by a later row.
    #[test]
    fn a_failed_write_marks_the_thread_and_stays_marked() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
        store
            .lock()
            .unwrap()
            .connection()
            .execute_batch("DROP TABLE thread_history_row;")
            .unwrap();
        let writer = StoreWriter::spawn(Arc::clone(&store), 8);
        let thread_id = ThreadId::mint();

        assert!(writer.enqueue(&thread_id, 1, source(1), message("doomed")));
        assert!(
            !writer.wait_for_writes(1, Duration::from_secs(2)),
            "the write cannot succeed"
        );
        let reason = writer.unsaved(&thread_id).expect("the thread is marked");
        assert!(reason.contains("could not write"), "{reason}");

        // A rebuild replaces the conversation, and that is what clears the mark.
        writer.clear_unsaved(&thread_id);
        assert_eq!(writer.unsaved(&thread_id), None);
        writer.flush().unwrap();
    }

    /// Shutting down stores what was accepted: the queue is drained before the
    /// writer stops, because a stop that dropped rows would be a silent loss.
    #[test]
    fn a_flush_stores_what_was_queued() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
        let writer = StoreWriter::spawn(Arc::clone(&store), 64);
        let thread_id = ThreadId::mint();
        for index in 1..=5 {
            assert!(writer.enqueue(
                &thread_id,
                index,
                source(index),
                message(&index.to_string())
            ));
        }
        writer.flush().unwrap();

        let store = store.lock().unwrap();
        assert_eq!(store.row_count(&thread_id).unwrap(), 5);
        let rows = store.rows(&thread_id).unwrap();
        assert_eq!(
            rows.iter().map(|row| row.seq).collect::<Vec<_>>(),
            vec![1, 2, 3, 4, 5],
            "the writer kept the order it was given"
        );
    }

    /// Rows for different threads do not interfere: one thread's failure leaves
    /// another's rows written, and the marks stay per thread.
    #[test]
    fn a_failure_is_reported_per_thread() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
        let writer = StoreWriter::spawn(Arc::clone(&store), 64);
        let failing = ThreadId::mint();
        let working = ThreadId::mint();

        // Drop the table after one row is stored, so the next write fails.
        assert!(writer.enqueue(&working, 1, source(1), message("kept")));
        assert!(writer.wait_for_writes(1, Duration::from_secs(2)));
        store
            .lock()
            .unwrap()
            .connection()
            .execute_batch("DROP TABLE thread_history_row;")
            .unwrap();
        assert!(writer.enqueue(&failing, 2, source(2), message("dropped")));

        let deadline = Instant::now() + Duration::from_secs(2);
        while writer.unsaved(&failing).is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(writer.unsaved(&failing).is_some());
        assert_eq!(writer.unsaved(&working), None, "the other thread is clean");
        writer.flush().unwrap();
    }
}
