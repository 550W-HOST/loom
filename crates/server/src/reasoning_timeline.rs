//! Coalescing streamed reasoning deltas into the timeline's thinking rows.
//!
//! A model's thinking reaches the log the same way its answer does: many
//! `item/reasoning/textDelta` frames that share one `itemId`. bb's `TimelineRow`
//! contract has one row per thinking item — the collapsed "Thought for 1.2s"
//! line a reader expands — so projecting one row per delta would spell the
//! reasoning out a token at a time.
//!
//! This is the seam for that fold, next to [`crate::assistant_timeline`] and for
//! the same reason: the folding rule belongs in one place that both the timeline
//! and anything else projecting the log can share, rather than in each reader.
//!
//! Nothing here decides whether thinking should be *shown*; that is the client's
//! preference. The fold only decides what one thinking item said and how long it
//! took, because a row that did not exist could never be shown at all — which is
//! exactly the report this module answers.

use std::collections::HashMap;

use loom_domain::{ProviderEvent, ThreadEventItem};
use serde_json::{json, Value};

/// The identity of one thinking item: the run that carried it and the provider's
/// item id within that run.
///
/// Both halves are needed for the same reason as an assistant message: the
/// worker's ACP translator is built per run, so `reasoning-v2-pi-msg-2` in one
/// turn is a different item from the same id in the next.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ReasoningItemId {
    /// The run the thinking streamed in.
    pub run_id: String,
    /// The provider's item id, unique within that run.
    pub item_id: String,
}

/// One thinking item accumulated from the deltas that carried it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReasoningItem {
    /// What identifies this item: run plus provider item id.
    pub id: ReasoningItemId,
    /// The streamed reasoning, in arrival order.
    pub text: String,
    /// The sequence of the first frame that carried text.
    pub start_sequence: u64,
    /// The sequence of the last frame that contributed to it.
    pub end_sequence: u64,
    /// When the first delta arrived.
    pub started_at_ms: u64,
    /// When the thinking ended: the frame in which the agent moved on.
    pub ended_at_ms: u64,
    /// `true` once the agent moved on, or reported the item complete.
    pub closed: bool,
}

impl ReasoningItem {
    /// How long the model thought, as the client would spell it.
    ///
    /// The client formats the same duration from the same two numbers, so the
    /// row a server builds and the row the client would build agree.
    pub fn duration_label(&self) -> String {
        duration_to_compact_string(self.ended_at_ms.saturating_sub(self.started_at_ms))
    }

    /// Whether this item has anything worth a row.
    ///
    /// An item that streamed only whitespace is dropped rather than rendered as
    /// an empty "Thought for 0s" line, which is what the client does with the
    /// same input.
    pub fn is_reportable(&self) -> bool {
        !self.text.trim().is_empty()
    }
}

/// The thinking items of one thread, keyed by [`ReasoningItemId`].
#[derive(Debug, Default)]
pub struct ReasoningTimeline {
    items: HashMap<ReasoningItemId, ReasoningItem>,
}

impl ReasoningTimeline {
    pub fn new() -> Self {
        Self::default()
    }

    /// Folds one run event in, returning `true` when this accumulator owns it.
    ///
    /// Owning an event means "this is reasoning, do not project a generic row
    /// for it" — including an empty delta, so the caller cannot fall through to
    /// a projection that has no row for reasoning at all.
    ///
    /// Every other event closes whatever thinking this run still has open,
    /// because that is what the agent moving on looks like from here: text
    /// starts, a tool is called, or the turn ends. The closing frame's timestamp
    /// is what makes the duration the *thinking's* rather than the turn's.
    pub fn absorb(
        &mut self,
        run_id: &str,
        event: &ProviderEvent,
        sequence: u64,
        at_ms: u64,
    ) -> bool {
        match event {
            ProviderEvent::ItemReasoningTextDelta { item_id, delta, .. }
            | ProviderEvent::ItemReasoningSummaryTextDelta { item_id, delta, .. } => {
                if !delta.is_empty() {
                    let item = self.entry(run_id, item_id, sequence, at_ms);
                    item.text.push_str(delta);
                    item.end_sequence = sequence;
                    item.ended_at_ms = at_ms;
                }
                true
            }
            // A provider that reports the whole item at the end rather than
            // streaming it: the text is the only source, and the item is done.
            ProviderEvent::ItemCompleted {
                item:
                    ThreadEventItem::Reasoning {
                        id,
                        summary,
                        content,
                        ..
                    },
                ..
            } => {
                let text: String = summary.iter().chain(content.iter()).cloned().collect();
                let item = self.entry(run_id, id, sequence, at_ms);
                if item.text.is_empty() {
                    item.text = text;
                }
                item.end_sequence = sequence;
                item.ended_at_ms = at_ms;
                item.closed = true;
                true
            }
            _ => {
                self.close_open_items(run_id, at_ms);
                false
            }
        }
    }

    /// Marks every still-open thinking item of one run as finished.
    fn close_open_items(&mut self, run_id: &str, at_ms: u64) {
        for item in self.items.values_mut().filter(|item| !item.closed) {
            if item.id.run_id != run_id {
                continue;
            }
            // A frame that arrives *before* the item's last delta cannot shorten
            // it: events are absorbed in sequence order, so this only guards
            // against a replayed frame arriving out of order.
            item.ended_at_ms = item.ended_at_ms.max(at_ms);
            item.closed = true;
        }
    }

    fn entry(
        &mut self,
        run_id: &str,
        item_id: &str,
        sequence: u64,
        at_ms: u64,
    ) -> &mut ReasoningItem {
        let id = ReasoningItemId {
            run_id: run_id.to_owned(),
            item_id: item_id.to_owned(),
        };
        self.items.entry(id.clone()).or_insert(ReasoningItem {
            id,
            text: String::new(),
            start_sequence: sequence,
            end_sequence: sequence,
            started_at_ms: at_ms,
            ended_at_ms: at_ms,
            closed: false,
        })
    }

    /// Every thinking item that has something to show, in source order.
    pub fn ordered(&self) -> Vec<&ReasoningItem> {
        let mut items: Vec<&ReasoningItem> = self
            .items
            .values()
            .filter(|item| item.is_reportable())
            .collect();
        items.sort_by_key(|item| (item.start_sequence, item.end_sequence));
        items
    }

    /// One item, when it streamed anything a row could show.
    pub fn get(&self, run_id: &str, item_id: &str) -> Option<&ReasoningItem> {
        self.items
            .get(&ReasoningItemId {
                run_id: run_id.to_owned(),
                item_id: item_id.to_owned(),
            })
            .filter(|item| item.is_reportable())
    }
}

/// The row fields for one thinking item.
///
/// Built here rather than at the call site so the timeline and any other reader
/// cannot disagree about what a thinking row looks like.
pub fn rows_for(items: &ReasoningTimeline, thread_id: &str) -> Vec<Value> {
    items
        .ordered()
        .into_iter()
        .map(|item| {
            json!({
                "id": format!("{}-reasoning-{}", item.id.run_id, item.id.item_id),
                "threadId": thread_id,
                "turnId": item.id.run_id,
                "sourceSeqStart": item.start_sequence,
                "sourceSeqEnd": item.end_sequence,
                "startedAt": item.started_at_ms,
                "createdAt": item.ended_at_ms,
                "completedAt": item.ended_at_ms,
                "kind": "system",
                "systemKind": "operation",
                "operationKind": "reasoning",
                // The client keys the expanded state of a thinking row on this.
                "reasoningId": item.id.item_id,
                "title": format!("Thought for {}", item.duration_label()),
                "detail": item.text,
                "status": "completed",
            })
        })
        .collect()
}

/// A duration the way the client spells it — `90ms`, `12s`, `1m 5s`.
///
/// The client's own rows use this format, so a server-built row ("Thought for
/// 1.2s") and a client-built one cannot read differently for the same thinking.
pub fn duration_to_compact_string(duration_ms: u64) -> String {
    if duration_ms < 1_000 {
        return format!("{duration_ms}ms");
    }
    let total_seconds = ((duration_ms as f64) / 1_000.0).round() as u64;
    if total_seconds < 60 {
        return format!("{total_seconds}s");
    }
    let hours = total_seconds / 3_600;
    let minutes = (total_seconds % 3_600) / 60;
    let seconds = total_seconds % 60;
    let mut parts: Vec<String> = Vec::new();
    if hours > 0 {
        parts.push(format!("{hours}h"));
    }
    if minutes > 0 {
        parts.push(format!("{minutes}m"));
    }
    if seconds > 0 {
        parts.push(format!("{seconds}s"));
    }
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn delta(item_id: &str, delta: &str) -> ProviderEvent {
        ProviderEvent::ItemReasoningTextDelta {
            item_id: item_id.to_owned(),
            delta: delta.to_owned(),
            provider_thread_id: "provider-thread".to_owned(),
            parent_tool_call_id: None,
        }
    }

    fn agent_delta(item_id: &str) -> ProviderEvent {
        ProviderEvent::ItemAgentMessageDelta {
            item_id: item_id.to_owned(),
            delta: "answer".to_owned(),
            provider_thread_id: "provider-thread".to_owned(),
            parent_tool_call_id: None,
        }
    }

    /// Thinking is one row per item, not one per delta, and it stops being open
    /// when the agent moves on to its answer.
    #[test]
    fn deltas_fold_into_one_item_and_the_answer_closes_it() {
        let mut timeline = ReasoningTimeline::new();
        assert!(timeline.absorb("run-1", &delta("r", "The"), 2, 1_000));
        assert!(timeline.absorb("run-1", &delta("r", " user"), 3, 1_100));
        assert!(timeline.absorb("run-1", &delta("r", " asked"), 4, 1_400));
        // The answer is not reasoning, but it is what ends the thinking.
        assert!(!timeline.absorb("run-1", &agent_delta("a"), 5, 1_900));

        let item = timeline.get("run-1", "r").expect("the item is reportable");
        assert_eq!(item.text, "The user asked");
        assert_eq!(item.start_sequence, 2);
        assert_eq!(item.end_sequence, 4);
        assert_eq!(item.duration_label(), "900ms");
        assert!(item.closed, "the answer ended the thinking");
    }

    /// A thinking item that streamed nothing worth reading gets no row at all,
    /// rather than an empty one.
    #[test]
    fn blank_thinking_is_not_a_row() {
        let mut timeline = ReasoningTimeline::new();
        timeline.absorb("run-1", &delta("r", "   "), 2, 1_000);
        timeline.absorb("run-1", &delta("r", "\n"), 3, 1_100);
        assert!(timeline.get("run-1", "r").is_none());
        assert!(rows_for(&timeline, "thread-1").is_empty());
    }

    /// A row carries what the contract requires, and the length the model spent
    /// thinking is measured between its first delta and the frame after it.
    #[test]
    fn a_row_states_the_contract_fields_and_the_duration() {
        let mut timeline = ReasoningTimeline::new();
        timeline.absorb("run-1", &delta("r", "Thinking"), 2, 1_000);
        timeline.absorb("run-1", &delta("r", " harder"), 3, 3_500);
        timeline.absorb("run-1", &agent_delta("a"), 4, 3_600);

        let rows = rows_for(&timeline, "thread-1");
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row["kind"], "system");
        assert_eq!(row["systemKind"], "operation");
        assert_eq!(row["operationKind"], "reasoning");
        assert_eq!(row["reasoningId"], "r");
        // The thinking ran from its first delta until the answer began, which is
        // the same pair of timestamps the client would have folded.
        assert_eq!(row["title"], "Thought for 3s");
        assert_eq!(row["detail"], "Thinking harder");
        assert_eq!(row["threadId"], "thread-1");
        assert_eq!(row["turnId"], "run-1");
        assert_eq!(row["sourceSeqStart"], 2);
        assert_eq!(row["sourceSeqEnd"], 3);
        assert_eq!(row["startedAt"], 1_000);
        assert_eq!(row["completedAt"], 3_600);
    }

    /// Another run's frames never close this run's thinking, which matters on a
    /// thread whose turns overlap in the log.
    #[test]
    fn another_runs_frames_do_not_close_this_runs_thinking() {
        let mut timeline = ReasoningTimeline::new();
        timeline.absorb("run-1", &delta("r", "Thinking"), 2, 1_000);
        timeline.absorb("run-2", &agent_delta("a"), 3, 2_000);
        let item = timeline.get("run-1", "r").expect("still reportable");
        assert!(!item.closed, "run-2's answer is not run-1's end");
        assert_eq!(item.ended_at_ms, 1_000);
    }

    /// The duration labels match the client's own formatter.
    #[test]
    fn durations_are_spelled_the_way_the_client_spells_them() {
        assert_eq!(duration_to_compact_string(0), "0ms");
        assert_eq!(duration_to_compact_string(999), "999ms");
        assert_eq!(duration_to_compact_string(1_000), "1s");
        assert_eq!(duration_to_compact_string(59_400), "59s");
        assert_eq!(duration_to_compact_string(60_000), "1m");
        assert_eq!(duration_to_compact_string(65_000), "1m 5s");
        assert_eq!(duration_to_compact_string(3_600_000), "1h");
        assert_eq!(duration_to_compact_string(3_665_000), "1h 1m 5s");
    }
}
