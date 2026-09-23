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
    pub started_at_ms: Option<u64>,
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
                | ThreadEventItem::WebSearch { .. }
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
            // A restored conversation has neither time: the agent's replay
            // carries no timestamp, and a load time is not the event's time.
            // Null is the honest answer, and the contract accepts it.
            "createdAt": self.completed_at_ms.or(self.started_at_ms),
            "kind": "work",
            "status": self.status(),
            "callId": self.id.item_id,
            "approvalStatus": Value::Null,
            "completedAt": self.completed_at_ms,
        })
    }
}

impl ToolActivity {
    /// This call as timeline rows.
    ///
    /// Usually one row: the kind of row follows the kind of item, because the
    /// client draws a shell command and a generic tool differently. A file
    /// change is the exception the contract already decided: one row per file,
    /// so a call that touched three files reads as three rows rather than one
    /// row listing three paths.
    pub fn rows(&self, thread_id: &str) -> Vec<Value> {
        if let ThreadEventItem::FileChange { changes, .. } = &self.item {
            // A file-edit call can be reported without its changes yet (an
            // approval still waiting, or an adapter that sends the call before
            // the diff). Falling back to the generic tool row keeps the call
            // visible instead of dropping it from the timeline.
            if changes.is_empty() {
                return vec![self.row(thread_id)];
            }
            return changes
                .iter()
                .enumerate()
                .map(|(index, change)| self.file_change_row(thread_id, index, change))
                .collect();
        }
        vec![self.row(thread_id)]
    }

    /// One row per file a change touched.
    ///
    /// `diff` follows the contract's own convention, which is not "always a
    /// unified diff": an added file carries its whole content and a deleted one
    /// the content it had, while an edit carries a patch. The stats follow from
    /// that — which is why they are computed here rather than trusted from the
    /// agent, whose idea of a diff is its own.
    fn file_change_row(
        &self,
        thread_id: &str,
        index: usize,
        change: &loom_domain::FileChange,
    ) -> Value {
        // Built from the shared base rather than from the tool row: the
        // contract's row variants are closed, and a `tool` row's fields
        // (`toolName`, `toolArgs`) are not what a `file-change` row carries.
        let mut row = self.base_fields(thread_id);
        let fields = row.as_object_mut().expect("a row is an object");
        // The contract's `file-change` variant closes its property set and does
        // not list `completedAt` (only the tool and command variants do), so the
        // shared base's copy is dropped rather than sent and rejected.
        fields.remove("completedAt");
        fields.insert("workKind".into(), json!("file-change"));
        let id = fields
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        fields.insert("id".into(), json!(format!("{id}:file-change:{index}")));
        let (added, removed) = file_change_diff_stats(change);
        fields.insert(
            "change".into(),
            json!({
                "path": change.path,
                "kind": change.kind,
                "movePath": change.move_path,
                "diff": change.diff,
                "diffStats": { "added": added, "removed": removed },
            }),
        );
        fields.insert("stdout".into(), Value::Null);
        fields.insert("stderr".into(), Value::Null);
        row
    }

    /// This call as one timeline row.
    fn row(&self, thread_id: &str) -> Value {
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
            // The kinds the contract gives their own row are drawn from the
            // item rather than flattened into a tool: a read names the file it
            // read, a search the query it ran. They carry no `approvalStatus`,
            // and their variants close their property sets, so the shared
            // base's copy is dropped rather than sent and rejected.
            ThreadEventItem::FileRead { path, cmd, .. } => {
                fields.remove("approvalStatus");
                fields.insert("workKind".into(), json!("file-read"));
                fields.insert("path".into(), json!(path));
                fields.insert("cmd".into(), json!(cmd));
            }
            ThreadEventItem::Search {
                mode,
                query,
                path,
                cmd,
                result_text,
                ..
            } => {
                fields.remove("approvalStatus");
                fields.insert("workKind".into(), json!("search"));
                fields.insert("mode".into(), json!(mode));
                fields.insert("query".into(), json!(query));
                fields.insert("path".into(), json!(path));
                fields.insert("cmd".into(), json!(cmd));
                if let Some(result_text) = result_text {
                    fields.insert("resultText".into(), json!(result_text));
                }
            }
            ThreadEventItem::WebSearch {
                queries,
                result_text,
                ..
            } => {
                fields.remove("approvalStatus");
                fields.insert("workKind".into(), json!("web-search"));
                fields.insert("queries".into(), json!(queries));
                if let Some(result_text) = result_text {
                    fields.insert("resultText".into(), json!(result_text));
                }
            }
            ThreadEventItem::WebFetch {
                url,
                prompt,
                pattern,
                result_text,
                ..
            } => {
                fields.remove("approvalStatus");
                fields.insert("workKind".into(), json!("web-fetch"));
                fields.insert("url".into(), json!(url));
                fields.insert("prompt".into(), json!(prompt));
                fields.insert("pattern".into(), json!(pattern));
                if let Some(result_text) = result_text {
                    fields.insert("resultText".into(), json!(result_text));
                }
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
        ThreadEventItem::WebSearch { .. } => "web_search".to_owned(),
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
        ThreadEventItem::WebSearch { .. } | ThreadEventItem::WebFetch { .. } => {
            ItemStatus::Completed
        }
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
            .or_else(|| result.as_ref().map(render_tool_result)),
        _ => None,
    }
}

/// A tool result as the row's output text.
///
/// The worker keeps a result it could read as text and leaves a structured value
/// as it arrived, so a string is the tool's own output and is shown verbatim —
/// `Value::to_string` would wrap it in JSON quotes and escape its newlines.
/// Anything else is indented rather than one compact line, because the row's
/// `<pre>` is the only place the value is read.
fn render_tool_result(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => serde_json::to_string_pretty(other).unwrap_or_else(|_| other.to_string()),
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
        at_ms: Option<u64>,
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
                activity.completed_at_ms = at_ms;
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
        at_ms: Option<u64>,
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
        | ThreadEventItem::WebSearch { id, .. }
        | ThreadEventItem::Search { id, .. }
        | ThreadEventItem::WebFetch { id, .. } => id,
        _ => "",
    }
}

/// The action a change describes, in the client's vocabulary.
///
/// A move with edits is an edit rather than a rename: the reader wants to know
/// the file changed, and the new path is on the change either way.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FileChangeAction {
    Created,
    Deleted,
    Renamed,
    Edited,
}

fn file_change_action(change: &loom_domain::FileChange) -> FileChangeAction {
    if change.move_path.is_some() {
        return if has_substantive_diff(change.diff.as_deref()) {
            FileChangeAction::Edited
        } else {
            FileChangeAction::Renamed
        };
    }
    match change.kind {
        loom_domain::FileChangeKind::Add => FileChangeAction::Created,
        loom_domain::FileChangeKind::Delete => FileChangeAction::Deleted,
        loom_domain::FileChangeKind::Update => FileChangeAction::Edited,
    }
}

/// Whether a diff has a line that actually changed.
fn has_substantive_diff(diff: Option<&str>) -> bool {
    let Some(diff) = diff else {
        return false;
    };
    diff.split('\n').any(|line| {
        !line.starts_with("+++ ")
            && !line.starts_with("--- ")
            && (line.starts_with('+') || line.starts_with('-'))
    })
}

/// A line that belongs to the patch's own frame rather than to the file.
fn is_patch_metadata_line(line: &str) -> bool {
    let line = line.trim_end();
    line.starts_with("diff --git ")
        || line.starts_with("index ")
        || line.starts_with("new file mode ")
        || line.starts_with("deleted file mode ")
        || line.starts_with("similarity index ")
        || line.starts_with("rename from ")
        || line.starts_with("rename to ")
        || line.starts_with("--- ")
        || line.starts_with("+++ ")
        || line.starts_with("@@")
        || line == "\\ No newline at end of file"
}

fn has_patch_metadata(diff: &str) -> bool {
    diff.split('\n').any(is_patch_metadata_line)
}

/// The lines a diff carries that are neither blank nor patch frame.
fn count_plain_content_lines(diff: &str) -> u64 {
    diff.split('\n')
        .map(str::trim_end)
        .filter(|line| !line.trim().is_empty())
        .filter(|line| !is_patch_metadata_line(line))
        .count() as u64
}

/// How many lines a change added and removed.
///
/// Ported from the client's own rule, because the client shows these numbers
/// and a second implementation that disagreed would show different totals for
/// the same edit depending on which side built the row. The rule is not
/// "count `+` and `-`": a whole added or deleted file carries its content
/// rather than a patch, so its lines are the count.
fn file_change_diff_stats(change: &loom_domain::FileChange) -> (u64, u64) {
    let Some(diff) = change.diff.as_deref() else {
        return (0, 0);
    };
    let action = file_change_action(change);
    let plain = count_plain_content_lines(diff);
    if !has_patch_metadata(diff)
        && matches!(
            action,
            FileChangeAction::Created | FileChangeAction::Deleted
        )
    {
        return match action {
            FileChangeAction::Created => (plain, 0),
            _ => (0, plain),
        };
    }

    let mut added = 0;
    let mut removed = 0;
    let mut saw_unified_line = false;
    for line in diff.split('\n') {
        if line.starts_with("+++ ") || line.starts_with("--- ") {
            continue;
        }
        if line.starts_with('+') {
            saw_unified_line = true;
            added += 1;
        } else if line.starts_with('-') {
            saw_unified_line = true;
            removed += 1;
        }
    }
    if saw_unified_line {
        return (added, removed);
    }
    match action {
        FileChangeAction::Created => (plain, 0),
        FileChangeAction::Deleted => (0, plain),
        FileChangeAction::Renamed | FileChangeAction::Edited => (0, 0),
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
        assert!(timeline.absorb("run-1", &started("call-1"), 4, Some(1_000)));
        assert!(timeline.absorb("run-1", &progress("call-1", "ls -la"), 5, Some(1_200)));
        assert!(timeline.absorb("run-1", &progress("call-1", "ls -la"), 6, Some(1_400)));
        assert!(timeline.absorb("run-1", &completed("call-1"), 7, Some(1_900)));

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

    /// A tool result reaches the row as its own text.
    ///
    /// The worker keeps a result it could read as text and leaves a structured
    /// value as it arrived, so a string result must arrive without JSON quotes
    /// and a structured one must be readable rather than one compact line.
    #[test]
    fn a_tool_result_reaches_the_row_as_text_or_indented_json() {
        let mut timeline = ToolTimeline::new();

        let mut text = tool_call("call-text", ItemStatus::Completed);
        if let ThreadEventItem::ToolCall { result, .. } = &mut text {
            *result = Some(serde_json::json!("repo: loom\nfiles: 1942\n"));
        }
        assert!(timeline.absorb(
            "run-1",
            &ProviderEvent::ItemCompleted {
                item: text,
                provider_thread_id: "provider-thread".to_owned(),
            },
            3,
            Some(1_100),
        ));
        let row = timeline
            .get("run-1", "call-text")
            .expect("the call exists")
            .row("thread-1");
        assert_eq!(row["output"], "repo: loom\nfiles: 1942\n");

        let mut structured = tool_call("call-json", ItemStatus::Completed);
        if let ThreadEventItem::ToolCall { result, .. } = &mut structured {
            *result = Some(serde_json::json!({"count": 3}));
        }
        assert!(timeline.absorb(
            "run-1",
            &ProviderEvent::ItemCompleted {
                item: structured,
                provider_thread_id: "provider-thread".to_owned(),
            },
            5,
            Some(1_300),
        ));
        let row = timeline
            .get("run-1", "call-json")
            .expect("the call exists")
            .row("thread-1");
        assert_eq!(row["output"], "{\n  \"count\": 3\n}");
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
            Some(1_000),
        );
        timeline.absorb(
            "run-1",
            &ProviderEvent::ItemCompleted {
                item,
                provider_thread_id: "provider-thread".to_owned(),
            },
            3,
            Some(2_200),
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

    /// A read, a search and a fetch are the contract's own row kinds. Each names
    /// what it touched; flattened into a tool row they showed a progress line
    /// where the file, the query or the URL belongs.
    #[test]
    fn a_read_a_search_and_a_fetch_carry_their_own_rows() {
        let mut timeline = ToolTimeline::new();
        let items = [
            ThreadEventItem::FileRead {
                id: "call-read".to_owned(),
                path: "/srv/project/src/a.rs".to_owned(),
                cmd: None,
                status: ItemStatus::Completed,
                presentation: None,
                parent_tool_call_id: None,
            },
            ThreadEventItem::Search {
                id: "call-search".to_owned(),
                mode: loom_domain::SearchMode::Content,
                query: "gbatch".to_owned(),
                path: Some("/srv/project".to_owned()),
                cmd: None,
                status: ItemStatus::Completed,
                result_text: None,
                presentation: None,
                parent_tool_call_id: None,
            },
            ThreadEventItem::WebFetch {
                id: "call-fetch".to_owned(),
                url: "https://example.com/doc".to_owned(),
                prompt: None,
                pattern: None,
                result_text: None,
                presentation: None,
                parent_tool_call_id: None,
            },
        ];
        for (index, item) in items.into_iter().enumerate() {
            timeline.absorb(
                "run-1",
                &ProviderEvent::ItemCompleted {
                    item,
                    provider_thread_id: "provider-thread".to_owned(),
                },
                10 + index as u64,
                Some(1_000),
            );
        }

        let read = row_for(&timeline, "call-read");
        assert_eq!(read["workKind"], "file-read");
        assert_eq!(read["path"], "/srv/project/src/a.rs");
        assert_eq!(read["cmd"], Value::Null);
        assert_eq!(read["callId"], "call-read");
        assert_keys(
            &read,
            &[
                "id",
                "threadId",
                "turnId",
                "sourceSeqStart",
                "sourceSeqEnd",
                "startedAt",
                "createdAt",
                "kind",
                "status",
                "workKind",
                "callId",
                "path",
                "cmd",
                "completedAt",
            ],
        );

        let search = row_for(&timeline, "call-search");
        assert_eq!(search["workKind"], "search");
        assert_eq!(search["mode"], "content");
        assert_eq!(search["query"], "gbatch");
        assert_eq!(search["path"], "/srv/project");
        assert_keys(
            &search,
            &[
                "id",
                "threadId",
                "turnId",
                "sourceSeqStart",
                "sourceSeqEnd",
                "startedAt",
                "createdAt",
                "kind",
                "status",
                "workKind",
                "callId",
                "mode",
                "query",
                "path",
                "cmd",
                "completedAt",
            ],
        );

        let fetch = row_for(&timeline, "call-fetch");
        assert_eq!(fetch["workKind"], "web-fetch");
        assert_eq!(fetch["url"], "https://example.com/doc");
        assert_eq!(fetch["prompt"], Value::Null);
        assert_eq!(fetch["pattern"], Value::Null);
        assert_keys(
            &fetch,
            &[
                "id",
                "threadId",
                "turnId",
                "sourceSeqStart",
                "sourceSeqEnd",
                "startedAt",
                "createdAt",
                "kind",
                "status",
                "workKind",
                "callId",
                "url",
                "prompt",
                "pattern",
                "completedAt",
            ],
        );
    }

    #[test]
    fn specialized_tool_results_reach_their_rows() {
        let mut timeline = ToolTimeline::new();
        for (sequence, item) in [
            ThreadEventItem::Search {
                id: "call-search-result".to_owned(),
                mode: loom_domain::SearchMode::Content,
                query: "needle".to_owned(),
                path: None,
                cmd: None,
                status: ItemStatus::Completed,
                result_text: Some("one match".to_owned()),
                presentation: None,
                parent_tool_call_id: None,
            },
            ThreadEventItem::WebFetch {
                id: "call-fetch-result".to_owned(),
                url: "https://example.com".to_owned(),
                prompt: None,
                pattern: None,
                result_text: Some("page text".to_owned()),
                presentation: None,
                parent_tool_call_id: None,
            },
        ]
        .into_iter()
        .enumerate()
        {
            timeline.absorb(
                "run-1",
                &ProviderEvent::ItemCompleted {
                    item,
                    provider_thread_id: "provider-thread".to_owned(),
                },
                20 + sequence as u64,
                Some(2_000),
            );
        }

        assert_eq!(
            row_for(&timeline, "call-search-result")["resultText"],
            "one match"
        );
        assert_eq!(
            row_for(&timeline, "call-fetch-result")["resultText"],
            "page text"
        );
    }

    fn row_for(timeline: &ToolTimeline, item_id: &str) -> Value {
        timeline
            .get("run-1", item_id)
            .expect("the call exists")
            .row("thread-1")
    }

    /// The row's fields, exactly: the contract's work variants close their
    /// property sets, so a field another kind carries (or one this kind does not
    /// declare) is a row the client is refused.
    fn assert_keys(row: &Value, expected: &[&str]) {
        let mut keys: Vec<&str> = row
            .as_object()
            .expect("a row is an object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        let mut expected = expected.to_vec();
        expected.sort_unstable();
        assert_eq!(keys, expected, "the row carries the wrong fields: {row}");
    }

    /// A failure is the contract's `error`, not its own word for it, and the
    /// row is still one row.
    #[test]
    fn a_failed_call_reports_the_contracts_status() {
        let mut timeline = ToolTimeline::new();
        timeline.absorb("run-1", &started("call-3"), 2, Some(1_000));
        timeline.absorb(
            "run-1",
            &ProviderEvent::ItemCompleted {
                item: tool_call("call-3", ItemStatus::Failed),
                provider_thread_id: "provider-thread".to_owned(),
            },
            3,
            Some(1_500),
        );

        let activity = timeline.get("run-1", "call-3").expect("the call exists");
        assert_eq!(activity.status(), "error");
        assert_eq!(activity.row("thread-1")["status"], "error");
    }

    fn file_change(changes: Vec<loom_domain::FileChange>, status: ItemStatus) -> ThreadEventItem {
        ThreadEventItem::FileChange {
            id: "call-edit".to_owned(),
            changes,
            status,
            approval_status: None,
            presentation: None,
            parent_tool_call_id: None,
        }
    }

    fn change(
        path: &str,
        kind: loom_domain::FileChangeKind,
        diff: &str,
    ) -> loom_domain::FileChange {
        loom_domain::FileChange {
            path: path.to_owned(),
            kind,
            move_path: None,
            diff: Some(diff.to_owned()),
        }
    }

    /// An edit is one row per file, and the patch the agent supplied is what the
    /// row carries — the client draws it, so loom must not paraphrase it.
    #[test]
    fn an_edit_becomes_one_row_per_file_with_its_patch() {
        let patch = "@@ -1,2 +1,3 @@\n context\n-removed\n+added\n+more\n";
        let mut timeline = ToolTimeline::new();
        timeline.absorb(
            "run-1",
            &ProviderEvent::ItemStarted {
                item: file_change(
                    vec![
                        change("src/a.rs", loom_domain::FileChangeKind::Update, patch),
                        change(
                            "src/b.rs",
                            loom_domain::FileChangeKind::Update,
                            "@@ -1 +1 @@\n-old\n+new\n",
                        ),
                    ],
                    ItemStatus::Pending,
                ),
                provider_thread_id: "provider-thread".to_owned(),
            },
            3,
            Some(1_000),
        );
        timeline.absorb(
            "run-1",
            &ProviderEvent::ItemCompleted {
                item: file_change(
                    vec![change(
                        "src/a.rs",
                        loom_domain::FileChangeKind::Update,
                        patch,
                    )],
                    ItemStatus::Completed,
                ),
                provider_thread_id: "provider-thread".to_owned(),
            },
            4,
            Some(1_500),
        );

        let activity = timeline.get("run-1", "call-edit").expect("the call exists");
        let rows = activity.rows("thread-1");
        assert_eq!(rows.len(), 1, "the completion replaced the item's changes");
        let row = &rows[0];
        assert_eq!(row["kind"], "work");
        assert_eq!(row["workKind"], "file-change");
        assert_eq!(row["callId"], "call-edit");
        assert_eq!(row["status"], "completed");
        assert_eq!(row["change"]["path"], "src/a.rs");
        assert_eq!(row["change"]["kind"], "update");
        assert_eq!(row["change"]["diff"], patch);
        // Two `+` lines and one `-`, and the `+++`/`---` frame is not content.
        assert_eq!(row["change"]["diffStats"]["added"], 2);
        assert_eq!(row["change"]["diffStats"]["removed"], 1);
        assert!(row["id"]
            .as_str()
            .is_some_and(|id| id.ends_with(":file-change:0")));
        // The contract's `file-change` variant closes its property set, so the
        // row carries exactly those fields — no `completedAt` and none of the
        // tool row's `toolName`/`toolArgs`/`output`, which the schema rejects.
        let mut keys: Vec<&str> = row
            .as_object()
            .expect("a row is an object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "approvalStatus",
                "callId",
                "change",
                "createdAt",
                "id",
                "kind",
                "sourceSeqEnd",
                "sourceSeqStart",
                "startedAt",
                "status",
                "stderr",
                "stdout",
                "threadId",
                "turnId",
                "workKind",
            ]
        );
    }

    /// A file-edit call reported without its changes is still a call: it falls
    /// back to a generic tool row rather than disappearing from the timeline.
    #[test]
    fn a_file_edit_without_changes_is_still_a_row() {
        let mut timeline = ToolTimeline::new();
        timeline.absorb(
            "run-1",
            &ProviderEvent::ItemStarted {
                item: file_change(Vec::new(), ItemStatus::Pending),
                provider_thread_id: "provider-thread".to_owned(),
            },
            6,
            Some(1_000),
        );

        let activity = timeline.get("run-1", "call-edit").expect("the call exists");
        let rows = activity.rows("thread-1");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["workKind"], "tool");
        assert_eq!(rows[0]["toolName"], "edit");
    }

    /// The contract's `diff` is not always a patch: a whole added or deleted
    /// file carries its content, and then the file's own lines are the count.
    #[test]
    fn a_whole_added_or_deleted_file_counts_its_lines() {
        let mut timeline = ToolTimeline::new();
        timeline.absorb(
            "run-1",
            &ProviderEvent::ItemStarted {
                item: file_change(
                    vec![
                        change(
                            "src/new.rs",
                            loom_domain::FileChangeKind::Add,
                            "fn one() {}\n\nfn two() {}\n",
                        ),
                        change(
                            "src/gone.rs",
                            loom_domain::FileChangeKind::Delete,
                            "fn old() {}\n",
                        ),
                    ],
                    ItemStatus::Completed,
                ),
                provider_thread_id: "provider-thread".to_owned(),
            },
            5,
            Some(1_000),
        );

        let activity = timeline.get("run-1", "call-edit").expect("the call exists");
        let rows = activity.rows("thread-1");
        assert_eq!(rows.len(), 2, "one row per file");
        assert_eq!(rows[0]["change"]["kind"], "add");
        assert_eq!(rows[0]["change"]["diffStats"]["added"], 2);
        assert_eq!(rows[0]["change"]["diffStats"]["removed"], 0);
        assert_eq!(rows[1]["change"]["kind"], "delete");
        assert_eq!(rows[1]["change"]["diffStats"]["added"], 0);
        assert_eq!(rows[1]["change"]["diffStats"]["removed"], 1);
        assert_ne!(rows[0]["id"], rows[1]["id"], "each file is its own row");
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
            Some(1_000),
        ));
        assert!(timeline.get("run-1", "assistant-1").is_none());
    }

    /// A restored conversation has no timestamps: the agent's replay carries
    /// none, and the moment the server loaded it is not the moment the call
    /// happened. Null is the honest answer, and it is a different thing from
    /// "this happened at the epoch".
    #[test]
    fn a_call_with_no_time_reports_null_times() {
        let mut timeline = ToolTimeline::new();
        timeline.absorb("restored-1", &started("call-1"), 1, None);
        timeline.absorb("restored-1", &completed("call-1"), 2, None);

        let activity = timeline
            .get("restored-1", "call-1")
            .expect("the call exists");
        let row = activity.row("thread-1");
        assert!(row["startedAt"].is_null(), "no fabricated start: {row}");
        assert!(row["createdAt"].is_null(), "no fabricated creation: {row}");
        assert!(
            row["completedAt"].is_null(),
            "no fabricated completion: {row}"
        );

        // The local grouping key is what a restored row carries as its turn.
        // It must be mistakable for a loom run id by nothing: the ambient
        // prefix rule is what keeps a run action from appearing on it.
        assert_eq!(row["turnId"], "restored-1");
        assert!(
            "restored-1".parse::<loom_domain::RunId>().is_err(),
            "a restored conversation's grouping key must not parse as a run id"
        );
    }
}
