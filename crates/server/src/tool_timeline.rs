//! Coalescing a tool call's frames into the timeline's work rows.
//!
//! A tool call is not one frame. ACP reports it as `item/started`, then any
//! number of `item/toolCall/progress` frames carrying what the agent is doing
//! (for a shell command, the command line itself), then `item/completed` with
//! the merged item. bb's `TimelineRow` contract has one row per call, so a row
//! per frame would show a single `ls` as four separate rows, and a reader that
//! took only `item/started` would show a nameless pending row.
//!
//! This module is that fold, beside [`crate::assistant_timeline`] and
//! [`crate::reasoning_timeline`]. Like them it is pure and holds no HTTP or
//! relay types, so the timeline is the only thing that has to agree with it.
//!
//! Rows are built for the item kinds the ACP adapter actually produces. A shell
//! command gets the contract's `command` row, which is the one that can show
//! the command line and its output; every other tool-ish item gets a `tool`
//! row, which is the honest generic shape — the specialised diff and file rows
//! need fields (a parsed diff, a read path) that ACP supplies as free-form tool
//! arguments, and inventing them from a progress string would be worse than
//! showing the call.

use std::collections::HashMap;

use loom_domain::{ItemStatus, ProviderEvent, ThreadEventItem};
use serde_json::{json, Value};

/// The identity of one tool call: the run that carried it and the provider's
/// item id within that run.
///
/// The provider's ids are unique only within a run — the ACP translator is
/// built per run — so both halves are needed to keep two turns' calls apart.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ToolActivityId {
    /// The run the call happened in.
    pub run_id: String,
    /// The provider's item id, unique within that run.
    pub item_id: String,
}

/// One tool call, accumulated from every frame that described it.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolActivity {
    /// What identifies this call: run plus provider item id.
    pub id: ToolActivityId,
    /// The most complete item seen: the start, then the merged completion.
    pub item: ThreadEventItem,
    /// The last progress message, which is where a command line arrives.
    pub progress: Option<String>,
    /// The sequence of the first frame that named this call.
    pub start_sequence: u64,
    /// The sequence of the last frame that contributed to it.
    pub end_sequence: u64,
    /// When the call started.
    pub started_at_ms: u64,
    /// When it finished, if it has.
    pub completed_at_ms: Option<u64>,
}

impl ToolActivity {
    /// Whether this item is a tool call at all.
    ///
    /// Everything the adapter builds from an ACP `ToolCall` qualifies; a
    /// reasoning or assistant item is another fold's business.
    pub fn is_tool(item: &ThreadEventItem) -> bool {
        matches!(
            item,
            ThreadEventItem::ToolCall { .. }
                | ThreadEventItem::CommandExecution { .. }
                | ThreadEventItem::FileChange { .. }
                | ThreadEventItem::FileRead { .. }
                | ThreadEventItem::Search { .. }
                | ThreadEventItem::WebFetch { .. }
        )
    }

    /// The contract's status for this item.
    pub fn status(&self) -> &'static str {
        match item_status(&self.item) {
            ItemStatus::Pending => "pending",
            ItemStatus::Completed => "completed",
            ItemStatus::Failed => "error",
            ItemStatus::Interrupted => "interrupted",
        }
    }

    /// The row fields every tool row shares.
    fn base_fields(&self, thread_id: &str) -> Value {
        json!({
            "id": format!("{}-tool-{}", self.id.run_id, self.id.item_id),
            "threadId": thread_id,
            "turnId": self.id.run_id,
            "sourceSeqStart": self.start_sequence,
            "sourceSeqEnd": self.end_sequence,
            "startedAt": self.started_at_ms,
            "createdAt": self.completed_at_ms.unwrap_or(self.started_at_ms),
            "kind": "work",
            "status": self.status(),
            "callId": self.id.item_id,
            "approvalStatus": Value::Null,
            "completedAt": self.completed_at_ms,
        })
    }
}

impl ToolActivity {
    /// This call as a timeline row.
    ///
    /// One row per call: the kind of row follows the kind of item, because the
    /// client draws a shell command and a generic tool differently.
    pub fn row(&self, thread_id: &str) -> Value {
        let base = self.base_fields(thread_id);
        let object = base.as_object().cloned().expect("a row is an object");
        let mut row = Value::Object(object);
        let fields = row.as_object_mut().expect("a row is an object");
        match &self.item {
            ThreadEventItem::CommandExecution {
                command,
                cwd,
                aggregated_output,
                exit_code,
                ..
            } => {
                fields.insert("workKind".into(), json!("command"));
                fields.insert("command".into(), json!(command));
                fields.insert("cwd".into(), json!(cwd));
                fields.insert("source".into(), Value::Null);
                fields.insert(
                    "output".into(),
                    json!(aggregated_output.clone().unwrap_or_default()),
                );
                fields.insert("exitCode".into(), json!(exit_code));
                fields.insert("activityIntents".into(), json!([]));
            }
            item => {
                fields.insert("workKind".into(), json!("tool"));
                fields.insert("toolName".into(), json!(tool_name(item)));
                fields.insert("toolArgs".into(), self.tool_args(item));
                fields.insert(
                    "output".into(),
                    json!(aggregated_output_of(item).unwrap_or_default()),
                );
            }
        }
        row
    }

    /// What the call was asked to do, in the shape the row carries.
    ///
    /// ACP gives a tool's arguments as free-form JSON the adapter cannot always
    /// read (it arrives after the start frame, or as an opaque string). The
    /// progress message is the fallback that is always there for a shell
    /// command, and the item's own arguments win when they exist because they
    /// are the agent's own spelling.
    fn tool_args(&self, item: &ThreadEventItem) -> Value {
        if let ThreadEventItem::ToolCall {
            arguments: Some(arguments),
            ..
        } = item
        {
            return json!(arguments);
        }
        match &self.progress {
            Some(progress) if !progress.trim().is_empty() => json!({ "progress": progress }),
            _ => Value::Null,
        }
    }
}

/// The name a tool row shows for an item that is not a shell command.
///
/// The provider's own word is carried through rather than mapped onto a label
/// loom invents: the client knows some tools by name, and a renamed one would
/// be a tool it could not recognise.
fn tool_name(item: &ThreadEventItem) -> String {
    match item {
        ThreadEventItem::ToolCall { tool, .. } => tool.clone(),
        ThreadEventItem::FileChange { .. } => "edit".to_owned(),
        ThreadEventItem::FileRead { .. } => "read".to_owned(),
        ThreadEventItem::Search { .. } => "search".to_owned(),
        ThreadEventItem::WebFetch { .. } => "fetch".to_owned(),
        _ => "tool".to_owned(),
    }
}

fn item_status(item: &ThreadEventItem) -> ItemStatus {
    match item {
        ThreadEventItem::ToolCall { status, .. }
        | ThreadEventItem::CommandExecution { status, .. }
        | ThreadEventItem::FileChange { status, .. }
        | ThreadEventItem::FileRead { status, .. }
        | ThreadEventItem::Search { status, .. } => *status,
        // A web fetch reports its result rather than a lifecycle status, so it
        // counts as finished the moment it exists.
        ThreadEventItem::WebFetch { .. } => ItemStatus::Completed,
        _ => ItemStatus::Pending,
    }
}

fn aggregated_output_of(item: &ThreadEventItem) -> Option<String> {
    match item {
        ThreadEventItem::CommandExecution {
            aggregated_output, ..
        } => aggregated_output.clone(),
        ThreadEventItem::ToolCall { result, error, .. } => error
            .clone()
            .or_else(|| result.as_ref().map(|value| value.to_string())),
        _ => None,
    }
}

/// The tool calls of one thread, keyed by [`ToolActivityId`].
#[derive(Debug, Default)]
pub struct ToolTimeline {
    items: HashMap<ToolActivityId, ToolActivity>,
}

impl ToolTimeline {
    pub fn new() -> Self {
        Self::default()
    }

    /// Folds one run event in, returning `true` when this accumulator owns it.
    ///
    /// Owning an event means "this is a tool call, do not project a generic row
    /// for it" — including a progress frame that carries nothing, so the caller
    /// cannot fall through to a projection with no row for a call.
    pub fn absorb(
        &mut self,
        run_id: &str,
        event: &ProviderEvent,
        sequence: u64,
        at_ms: u64,
    ) -> bool {
        match event {
            ProviderEvent::ItemStarted { item, .. } if ToolActivity::is_tool(item) => {
                let activity = self.entry(run_id, item_id_of(item), sequence, at_ms);
                if activity.start_sequence == sequence {
                    activity.item = item.clone();
                }
                true
            }
            ProviderEvent::ItemToolCallProgress {
                item_id, message, ..
            } => {
                let activity = self.entry(run_id, item_id, sequence, at_ms);
                if let Some(message) = message {
                    if !message.trim().is_empty() {
                        activity.progress = Some(message.clone());
                    }
                }
                activity.end_sequence = sequence;
                true
            }
            ProviderEvent::ItemCompleted { item, .. } if ToolActivity::is_tool(item) => {
                let activity = self.entry(run_id, item_id_of(item), sequence, at_ms);
                activity.item = item.clone();
                activity.end_sequence = sequence;
                activity.completed_at_ms = Some(at_ms);
                true
            }
            _ => false,
        }
    }

    fn entry(
        &mut self,
        run_id: &str,
        item_id: &str,
        sequence: u64,
        at_ms: u64,
    ) -> &mut ToolActivity {
        let id = ToolActivityId {
            run_id: run_id.to_owned(),
            item_id: item_id.to_owned(),
        };
        self.items
            .entry(id.clone())
            .or_insert_with(|| ToolActivity {
                id,
                item: ThreadEventItem::ToolCall {
                    id: item_id.to_owned(),
                    server: None,
                    tool: "tool".to_owned(),
                    arguments: None,
                    status: ItemStatus::Pending,
                    result: None,
                    error: None,
                    duration_ms: None,
                    presentation: None,
                    parent_tool_call_id: None,
                },
                progress: None,
                start_sequence: sequence,
                end_sequence: sequence,
                started_at_ms: at_ms,
                completed_at_ms: None,
            })
    }

    /// One call, when it has anything a row could show.
    pub fn get(&self, run_id: &str, item_id: &str) -> Option<&ToolActivity> {
        self.items.get(&ToolActivityId {
            run_id: run_id.to_owned(),
            item_id: item_id.to_owned(),
        })
    }
}

/// The item id a tool item carries.
fn item_id_of(item: &ThreadEventItem) -> &str {
    match item {
        ThreadEventItem::ToolCall { id, .. }
        | ThreadEventItem::CommandExecution { id, .. }
        | ThreadEventItem::FileChange { id, .. }
        | ThreadEventItem::FileRead { id, .. }
        | ThreadEventItem::Search { id, .. }
        | ThreadEventItem::WebFetch { id, .. } => id,
        _ => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_call(id: &str, status: ItemStatus) -> ThreadEventItem {
        ThreadEventItem::ToolCall {
            id: id.to_owned(),
            server: None,
            tool: "execute".to_owned(),
            arguments: None,
            status,
            result: None,
            error: None,
            duration_ms: None,
            presentation: None,
            parent_tool_call_id: None,
        }
    }

    fn started(id: &str) -> ProviderEvent {
        ProviderEvent::ItemStarted {
            item: tool_call(id, ItemStatus::Pending),
            provider_thread_id: "provider-thread".to_owned(),
        }
    }

    fn progress(id: &str, message: &str) -> ProviderEvent {
        ProviderEvent::ItemToolCallProgress {
            item_id: id.to_owned(),
            message: Some(message.to_owned()),
            provider_thread_id: "provider-thread".to_owned(),
            parent_tool_call_id: None,
        }
    }

    fn completed(id: &str) -> ProviderEvent {
        ProviderEvent::ItemCompleted {
            item: tool_call(id, ItemStatus::Completed),
            provider_thread_id: "provider-thread".to_owned(),
        }
    }

    /// A call is one row, and the command line the agent reported on the way is
    /// what the row carries — the start frame alone knows nothing about it.
    #[test]
    fn a_calls_frames_fold_into_one_row_that_names_the_command() {
        let mut timeline = ToolTimeline::new();
        assert!(timeline.absorb("run-1", &started("call-1"), 4, 1_000));
        assert!(timeline.absorb("run-1", &progress("call-1", "ls -la"), 5, 1_200));
        assert!(timeline.absorb("run-1", &progress("call-1", "ls -la"), 6, 1_400));
        assert!(timeline.absorb("run-1", &completed("call-1"), 7, 1_900));

        let activity = timeline.get("run-1", "call-1").expect("the call exists");
        assert_eq!(activity.status(), "completed");
        assert_eq!(activity.start_sequence, 4);
        assert_eq!(activity.end_sequence, 7);
        assert_eq!(activity.completed_at_ms, Some(1_900));

        let row = activity.row("thread-1");
        assert_eq!(row["kind"], "work");
        assert_eq!(row["workKind"], "tool");
        assert_eq!(row["callId"], "call-1");
        assert_eq!(row["toolName"], "execute");
        assert_eq!(row["status"], "completed");
        assert_eq!(row["toolArgs"]["progress"], "ls -la");
        assert_eq!(row["turnId"], "run-1");
    }

    /// A shell command gets the command row: it is the one that can show the
    /// command, the directory and the exit code.
    #[test]
    fn a_shell_command_becomes_a_command_row() {
        let mut timeline = ToolTimeline::new();
        let item = ThreadEventItem::CommandExecution {
            id: "call-2".to_owned(),
            command: "cargo test".to_owned(),
            cwd: "/srv/project".to_owned(),
            status: ItemStatus::Completed,
            approval_status: None,
            aggregated_output: Some("ok".to_owned()),
            exit_code: Some(0),
            duration_ms: Some(1_200.0),
            presentation: None,
            parent_tool_call_id: None,
        };
        timeline.absorb(
            "run-1",
            &ProviderEvent::ItemStarted {
                item: item.clone(),
                provider_thread_id: "provider-thread".to_owned(),
            },
            2,
            1_000,
        );
        timeline.absorb(
            "run-1",
            &ProviderEvent::ItemCompleted {
                item,
                provider_thread_id: "provider-thread".to_owned(),
            },
            3,
            2_200,
        );

        let row = timeline
            .get("run-1", "call-2")
            .expect("the call exists")
            .row("thread-1");
        assert_eq!(row["workKind"], "command");
        assert_eq!(row["command"], "cargo test");
        assert_eq!(row["cwd"], "/srv/project");
        assert_eq!(row["output"], "ok");
        assert_eq!(row["exitCode"], 0);
        assert_eq!(row["completedAt"], 2_200);
        assert_eq!(row["activityIntents"], json!([]));
    }

    /// A failure is the contract's `error`, not its own word for it, and the
    /// row is still one row.
    #[test]
    fn a_failed_call_reports_the_contracts_status() {
        let mut timeline = ToolTimeline::new();
        timeline.absorb("run-1", &started("call-3"), 2, 1_000);
        timeline.absorb(
            "run-1",
            &ProviderEvent::ItemCompleted {
                item: tool_call("call-3", ItemStatus::Failed),
                provider_thread_id: "provider-thread".to_owned(),
            },
            3,
            1_500,
        );

        let activity = timeline.get("run-1", "call-3").expect("the call exists");
        assert_eq!(activity.status(), "error");
        assert_eq!(activity.row("thread-1")["status"], "error");
    }

    /// Reasoning and answers are other folds' events; this one must not claim
    /// them or the caller would drop their rows.
    #[test]
    fn other_items_are_not_claimed() {
        let mut timeline = ToolTimeline::new();
        assert!(!timeline.absorb(
            "run-1",
            &ProviderEvent::ItemAgentMessageDelta {
                item_id: "assistant-1".to_owned(),
                delta: "hi".to_owned(),
                provider_thread_id: "provider-thread".to_owned(),
                parent_tool_call_id: None,
            },
            2,
            1_000,
        ));
        assert!(timeline.get("run-1", "assistant-1").is_none());
    }
}
