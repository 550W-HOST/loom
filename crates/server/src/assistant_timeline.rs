//! Coalescing streamed assistant deltas into the timeline's message rows.
//!
//! The run contract carries an assistant answer as many `item/agentMessage/delta`
//! frames that share one `itemId` (and the terminal `item/completed` for that
//! same id). bb's `TimelineRow` contract, however, has one **conversation row
//! per message**, so projecting one row per delta would show a message chopped
//! into its chunks.
//!
//! This module is the seam: [`AssistantMessageTimeline`] folds a thread's run
//! events into per-item rows, keyed by item id, and only materializes them when
//! asked. It is deliberately pure and holds no relay, registry or HTTP types,
//! so both `threads.timeline` and `threads.output` can build on the same
//! accumulator instead of restating the folding rule.

use std::collections::HashMap;

use loom_domain::ProviderEvent;
use serde_json::{json, Value};

/// The identity of one assistant message: the run that carried it and the
/// provider's item id within that run.
///
/// Both halves are needed. The worker's ACP translator is constructed per run,
/// so its item ids (`assistant-1`, `assistant-v2-<message>`) are unique only
/// *within* a run; a thread's second turn streaming `assistant-1` again is a
/// different message, not a continuation of the first.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AssistantMessageId {
    /// The run the message streamed in.
    pub run_id: String,
    /// The provider's item id, unique within that run.
    pub item_id: String,
}

/// One assistant message accumulated from the deltas that carried it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssistantMessage {
    /// What identifies this message: run plus provider item id.
    pub id: AssistantMessageId,
    /// The streamed text, in arrival order.
    pub text: String,
    /// The sequence of the first frame that carried text for this message: the
    /// timeline row's sort key, and what keeps messages in stream order.
    pub start_sequence: u64,
    /// The sequence of the last frame that contributed to the message.
    pub end_sequence: u64,
    /// `true` once an `item/completed` for this message arrived.
    pub completed: bool,
}

impl AssistantMessage {
    /// The row fields shared by every assistant conversation row.
    ///
    /// Built here rather than at each call site so the timeline and the output
    /// projection cannot disagree about what an assistant message row looks
    /// like.
    pub fn row_fields(&self) -> Vec<(&'static str, Value)> {
        vec![
            ("kind", json!("conversation")),
            ("text", json!(self.text)),
            ("attachments", Value::Null),
            ("role", json!("assistant")),
            ("turnRequest", Value::Null),
        ]
    }
}

/// The assistant messages of one thread, keyed by [`AssistantMessageId`].
///
/// A message exists once it has **text**, so the streaming frame that carries
/// the first characters is what creates it. An empty delta therefore creates
/// nothing (and claims nothing), which is what lets the frame that does carry
/// text — or a completion that reports a whole message at once — be the frame
/// that opens the message.
///
/// Insertion order is not the authority for output order;
/// [`AssistantMessageTimeline::ordered`] sorts on the recorded sequences. That
/// is deliberate: a duplicate frame (a relay replay, a retried page) folds into
/// the same message instead of opening a second one, which only a keyed
/// accumulator gives.
#[derive(Debug, Default)]
pub struct AssistantMessageTimeline {
    messages: HashMap<AssistantMessageId, AssistantMessage>,
}

impl AssistantMessageTimeline {
    pub fn new() -> Self {
        Self::default()
    }

    /// Folds one run event in, returning `true` when it was an assistant frame
    /// — an `item/agentMessage/delta` or the completion of an assistant
    /// message — i.e. when this accumulator owns the event.
    ///
    /// The return value is independent of whether text was recorded: an empty
    /// delta is still an assistant frame, and answering `false` would make the
    /// caller fall through to a projection that has no row for it.
    pub fn absorb(&mut self, run_id: &str, event: &ProviderEvent, sequence: u64) -> bool {
        match event {
            ProviderEvent::ItemAgentMessageDelta { item_id, delta, .. } => {
                if !delta.is_empty() {
                    let message = self.entry(run_id, item_id, sequence);
                    message.text.push_str(delta);
                    message.end_sequence = sequence;
                }
                true
            }
            ProviderEvent::ItemCompleted { item, .. } => {
                let (item_id, text) = match item {
                    loom_domain::ThreadEventItem::AgentMessage { id, text, .. } => {
                        (id.clone(), text.clone())
                    }
                    _ => return false,
                };
                let id = AssistantMessageId {
                    run_id: run_id.to_owned(),
                    item_id: item_id.clone(),
                };
                if self.messages.contains_key(&id) {
                    let message = self.entry(run_id, &item_id, sequence);
                    // The completion's text is the provider's own snapshot of
                    // the whole message; the streamed deltas are already it, so
                    // it is only used when nothing streamed.
                    if message.text.is_empty() && !text.is_empty() {
                        message.text = text;
                    }
                    message.end_sequence = sequence;
                    message.completed = true;
                } else if !text.is_empty() {
                    // A whole message reported without deltas: it never
                    // streamed, and the completion is its only source.
                    let message = self.entry(run_id, &item_id, sequence);
                    message.text = text;
                    message.end_sequence = sequence;
                    message.completed = true;
                }
                // A completion with no text and nothing streamed says only that
                // an empty message existed. A row for it would be an empty
                // conversation bubble, so none is created.
                true
            }
            _ => false,
        }
    }

    fn entry(&mut self, run_id: &str, item_id: &str, sequence: u64) -> &mut AssistantMessage {
        let id = AssistantMessageId {
            run_id: run_id.to_owned(),
            item_id: item_id.to_owned(),
        };
        self.messages
            .entry(id.clone())
            .or_insert_with(|| AssistantMessage {
                id,
                text: String::new(),
                start_sequence: sequence,
                end_sequence: sequence,
                completed: false,
            })
    }

    /// Every message, in source order (the first frame that carried text).
    pub fn ordered(&self) -> Vec<&AssistantMessage> {
        let mut messages: Vec<&AssistantMessage> = self.messages.values().collect();
        messages.sort_by_key(|message| (message.start_sequence, message.end_sequence));
        messages
    }

    /// The concatenated streamed answer, which is what `threads.output` serves.
    ///
    /// Empty deltas were dropped by [`Self::absorb`], so this is the text a
    /// client would have assembled from the frames it received.
    pub fn concatenated_text(&self) -> String {
        self.ordered()
            .into_iter()
            .map(|message| message.text.as_str())
            .collect::<Vec<_>>()
            .join("")
    }

    /// Whether any assistant text was seen at all.
    ///
    /// `threads.output` uses this to choose between the streamed text and the
    /// text of the last completion, which is what it did before this module
    /// existed and what the contract still expects.
    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    /// The messages of one run, in source order.
    pub fn get(&self, run_id: &str, item_id: &str) -> Option<&AssistantMessage> {
        self.messages.get(&AssistantMessageId {
            run_id: run_id.to_owned(),
            item_id: item_id.to_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use loom_domain::{ProviderEvent, ThreadEventItem};

    fn delta(item_id: &str, text: &str) -> ProviderEvent {
        ProviderEvent::ItemAgentMessageDelta {
            item_id: item_id.to_owned(),
            delta: text.to_owned(),
            provider_thread_id: "ptid".into(),
            parent_tool_call_id: None,
        }
    }

    fn completed(item_id: &str, text: &str) -> ProviderEvent {
        ProviderEvent::ItemCompleted {
            item: ThreadEventItem::AgentMessage {
                id: item_id.to_owned(),
                text: text.to_owned(),
                presentation: None,
                parent_tool_call_id: None,
            },
            provider_thread_id: "ptid".into(),
        }
    }

    #[test]
    fn deltas_sharing_an_item_id_become_one_message() {
        let mut timeline = AssistantMessageTimeline::new();
        for (index, text) in ["Hi", ".", " What", " would"].iter().enumerate() {
            assert!(timeline.absorb("run-1", &delta("assistant-1", text), index as u64 + 1));
        }
        let messages = timeline.ordered();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].text, "Hi. What would");
        assert_eq!(messages[0].start_sequence, 1);
        assert_eq!(messages[0].end_sequence, 4);
        assert!(!messages[0].completed);
    }

    #[test]
    fn a_duplicate_delta_folds_into_the_same_message() {
        let mut timeline = AssistantMessageTimeline::new();
        timeline.absorb("run-1", &delta("assistant-1", "hello"), 1);
        // A replayed frame: same run, same item, same sequence.
        timeline.absorb("run-1", &delta("assistant-1", "hello"), 1);
        assert_eq!(timeline.ordered().len(), 1);
        assert_eq!(timeline.ordered()[0].text, "hellohello");
    }

    #[test]
    fn separate_item_ids_are_separate_messages_in_stream_order() {
        let mut timeline = AssistantMessageTimeline::new();
        timeline.absorb("run-1", &delta("assistant-2", "second"), 5);
        timeline.absorb("run-1", &delta("assistant-1", "first "), 1);
        let messages = timeline.ordered();
        assert_eq!(
            messages
                .iter()
                .map(|message| message.text.as_str())
                .collect::<Vec<_>>(),
            vec!["first ", "second"]
        );
    }

    #[test]
    fn the_same_item_id_in_a_later_run_is_a_different_message() {
        // The worker's translator is built per run, so `assistant-1` is reused
        // by the next turn. Folding the two runs together would merge two
        // answers into one bubble.
        let mut timeline = AssistantMessageTimeline::new();
        timeline.absorb("run-1", &delta("assistant-1", "first answer"), 1);
        timeline.absorb("run-2", &delta("assistant-1", "second answer"), 9);
        let messages = timeline.ordered();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].text, "first answer");
        assert_eq!(messages[1].text, "second answer");
    }

    #[test]
    fn a_completion_without_text_does_not_erase_the_deltas() {
        let mut timeline = AssistantMessageTimeline::new();
        timeline.absorb("run-1", &delta("assistant-1", "the answer"), 1);
        assert!(timeline.absorb("run-1", &completed("assistant-1", ""), 2));
        let messages = timeline.ordered();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].text, "the answer");
        assert!(messages[0].completed);
    }

    #[test]
    fn a_completion_without_deltas_still_produces_a_message() {
        let mut timeline = AssistantMessageTimeline::new();
        assert!(timeline.absorb("run-1", &completed("assistant-1", "whole message"), 3));
        let messages = timeline.ordered();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].text, "whole message");
        assert_eq!(messages[0].start_sequence, 3);
        assert!(messages[0].completed);
    }

    #[test]
    fn an_empty_completion_without_deltas_creates_no_message() {
        // The worker flushes an assistant item with empty text because the
        // deltas already carried the content. When there were no deltas either,
        // there is no message, and a row for it would be an empty bubble.
        let mut timeline = AssistantMessageTimeline::new();
        assert!(timeline.absorb("run-1", &completed("assistant-1", ""), 2));
        assert!(timeline.is_empty());
    }

    #[test]
    fn concatenated_text_is_the_answers_in_stream_order() {
        let mut timeline = AssistantMessageTimeline::new();
        timeline.absorb("run-1", &delta("assistant-1", "one"), 1);
        timeline.absorb("run-2", &delta("assistant-1", "two"), 2);
        assert_eq!(timeline.concatenated_text(), "onetwo");
        assert!(!timeline.is_empty());
    }

    #[test]
    fn an_empty_delta_is_an_assistant_frame_but_adds_no_message() {
        // The frame is claimed, so the caller does not fall through to a
        // projection with no row for it; and no message exists, so the frame
        // that carries the text is the one that opens the row.
        let mut timeline = AssistantMessageTimeline::new();
        assert!(timeline.absorb("run-1", &delta("assistant-1", ""), 1));
        assert!(timeline.is_empty());
        assert!(timeline.absorb("run-1", &delta("assistant-1", "late text"), 2));
        let messages = timeline.ordered();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].text, "late text");
        assert_eq!(messages[0].start_sequence, 2);
    }

    #[test]
    fn a_non_assistant_event_is_not_absorbed() {
        let mut timeline = AssistantMessageTimeline::new();
        let event = ProviderEvent::TurnStarted {
            provider_thread_id: "ptid".into(),
            parent_tool_call_id: None,
        };
        assert!(!timeline.absorb("run-1", &event, 1));
        assert!(timeline.is_empty());
    }

    #[test]
    fn the_terminal_empty_delta_row_is_not_a_second_message() {
        // `ConversationMessageContent` and the timeline both key on the item,
        // so a completed item that carries no text must not add a row.
        let mut timeline = AssistantMessageTimeline::new();
        timeline.absorb("run-1", &delta("assistant-1", "Hi there"), 1);
        timeline.absorb("run-1", &completed("assistant-1", ""), 2);
        let messages = timeline.ordered();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].text, "Hi there");
    }
}
