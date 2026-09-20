//! The translator's behaviour, asserted directly against its output.
//!
//! These drive the translation without an agent, an event loop or a transport:
//! they build ACP values and check the contract bodies that come out. That is
//! the point of keeping the translator free of I/O.

use agent_client_protocol_schema::v1::{
    ContentBlock, ContentChunk, Plan, PlanEntry, PlanEntryPriority, PlanEntryStatus,
    SessionInfoUpdate, SessionUpdate, StopReason, TextContent, ToolCall, ToolCallContent,
    ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields, ToolKind,
};
use agent_client_protocol_schema::v2;
use loom_domain::{ItemStatus, ProviderEvent, ThreadEventItem, TurnStatus};

use super::{AcpTranslator, RunContext};

fn translator() -> AcpTranslator {
    AcpTranslator::new(RunContext {
        thread_id: loom_domain::ThreadId::mint(),
        cwd: Some("/srv/project".into()),
        provider_session_id: None,
    })
}

fn text_chunk(text: &str) -> ContentChunk {
    ContentChunk::new(ContentBlock::Text(TextContent::new(text.to_string())))
}

/// The item id a delta refers to.
fn delta_id(event: &ProviderEvent) -> String {
    match event {
        ProviderEvent::ItemAgentMessageDelta { item_id, .. } => item_id.clone(),
        other => panic!("expected an agent message delta, got {other:?}"),
    }
}

/// A tool call with the given kind and arguments.
fn tool_call(kind: ToolKind, raw_input: Option<serde_json::Value>) -> ToolCall {
    let call = ToolCall::new("call-1", "a tool").kind(kind);
    match raw_input {
        Some(input) => call.raw_input(input),
        None => call,
    }
}

// --- rule 3: exactly one terminal event -----------------------------------

#[test]
fn every_stop_reason_produces_exactly_one_terminal_event() {
    for (reason, expected) in [
        (StopReason::EndTurn, TurnStatus::Completed),
        // A limit was reached, so the model stopped and the turn succeeded.
        (StopReason::MaxTokens, TurnStatus::Completed),
        (StopReason::MaxTurnRequests, TurnStatus::Completed),
        (StopReason::Refusal, TurnStatus::Failed),
        (StopReason::Cancelled, TurnStatus::Interrupted),
    ] {
        let mut t = translator();
        t.on_prompt_sent();
        let events = t.on_stop_reason(reason);
        let terminals = events
            .iter()
            .filter(|e| matches!(e, ProviderEvent::TurnCompleted { .. }))
            .count();
        assert_eq!(terminals, 1, "one terminal event for {reason:?}");
        match events.last() {
            Some(ProviderEvent::TurnCompleted { status, .. }) => {
                assert_eq!(*status, expected, "status for {reason:?}");
            }
            other => panic!("last event must be the terminal, got {other:?}"),
        }
    }
}

#[test]
fn a_failure_also_ends_the_turn() {
    let mut t = translator();
    t.on_prompt_sent();
    let events = t.on_failure("transport closed".into());
    match events.last() {
        Some(ProviderEvent::TurnCompleted { status, error, .. }) => {
            assert_eq!(*status, TurnStatus::Failed);
            assert_eq!(
                error.as_ref().map(|e| e.message.as_str()),
                Some("transport closed")
            );
        }
        other => panic!("expected a terminal event, got {other:?}"),
    }
}

// --- rule 2: user text is not an assistant delta ---------------------------

#[test]
fn a_user_chunk_never_produces_an_agent_message_delta() {
    let mut t = translator();
    let events = t.on_session_update(&SessionUpdate::UserMessageChunk(text_chunk("hello")));

    assert!(
        !events
            .iter()
            .any(|e| matches!(e, ProviderEvent::ItemAgentMessageDelta { .. })),
        "the user's own text must not share the assistant's channel: {events:?}"
    );
    match events.first() {
        Some(ProviderEvent::ItemStarted { item, .. }) => match item {
            ThreadEventItem::UserMessage { content, .. } => {
                assert_eq!(content.len(), 1, "the text is carried: {content:?}");
            }
            other => panic!("expected a user message item, got {other:?}"),
        },
        other => panic!("expected an item start, got {other:?}"),
    }
}

// --- rule 1: data-driven variant selection ---------------------------------

#[test]
fn an_execute_call_with_a_command_becomes_a_command_execution() {
    let mut t = translator();
    let call = tool_call(
        ToolKind::Execute,
        Some(serde_json::json!({"command": "ls"})),
    );
    let events = t.on_session_update(&SessionUpdate::ToolCall(call));

    match events.iter().find_map(|e| match e {
        ProviderEvent::ItemStarted { item, .. } => Some(item),
        _ => None,
    }) {
        Some(ThreadEventItem::CommandExecution { command, cwd, .. }) => {
            assert_eq!(command, "ls");
            // The dispatch's cwd is the only source when the call omits one.
            assert_eq!(cwd, "/srv/project");
        }
        other => panic!("expected a command execution, got {other:?}"),
    }
}

#[test]
fn the_same_kind_without_its_data_falls_back_to_the_generic_tool_item() {
    // This is the rule most likely to rot, so both sides are asserted: the
    // kind alone must not decide the variant.
    let mut t = translator();
    let call = tool_call(ToolKind::Execute, None);
    let events = t.on_session_update(&SessionUpdate::ToolCall(call));

    match events.iter().find_map(|e| match e {
        ProviderEvent::ItemStarted { item, .. } => Some(item),
        _ => None,
    }) {
        Some(ThreadEventItem::ToolCall { tool, .. }) => {
            assert_eq!(tool, "execute", "the kind is still recorded");
        }
        other => panic!(
            "an execute call with no command must not become a command execution \
             (that would fabricate the command line): {other:?}"
        ),
    }
}

#[test]
fn a_search_call_without_a_query_is_generic_too() {
    let mut t = translator();
    let call = tool_call(ToolKind::Search, Some(serde_json::json!({"pattern": "x"})));
    let events = t.on_session_update(&SessionUpdate::ToolCall(call));

    assert!(
        !events.iter().any(|e| matches!(
            e,
            ProviderEvent::ItemStarted {
                item: ThreadEventItem::Search { .. },
                ..
            }
        )),
        "no `query` means no Search item: {events:?}"
    );
}

#[test]
fn an_edit_with_a_diff_becomes_a_file_change() {
    let mut t = translator();
    let mut call = tool_call(ToolKind::Edit, None);
    call.content = vec![ToolCallContent::Diff(
        agent_client_protocol_schema::v1::Diff::new("/srv/a.rs", "new text").old_text("old text"),
    )];
    let events = t.on_session_update(&SessionUpdate::ToolCall(call));

    match events.iter().find_map(|e| match e {
        ProviderEvent::ItemStarted { item, .. } => Some(item),
        _ => None,
    }) {
        Some(ThreadEventItem::FileChange { changes, .. }) => {
            assert_eq!(changes.len(), 1);
            assert_eq!(changes[0].path, "/srv/a.rs");
            assert!(
                matches!(changes[0].kind, loom_domain::FileChangeKind::Update),
                "an existing file is an update, not an add"
            );
        }
        other => panic!("expected a file change, got {other:?}"),
    }
}

#[test]
fn an_edit_carries_a_patch_rather_than_the_whole_new_file() {
    // v1 hands over the two texts, and the timeline draws a patch: a row whose
    // `diff` was the file's new content would show a diff view in which nothing
    // is marked as changed.
    let mut t = translator();
    let mut call = tool_call(ToolKind::Edit, None);
    call.content = vec![ToolCallContent::Diff(
        agent_client_protocol_schema::v1::Diff::new("/srv/a.rs", "one\ntwo changed\nthree\n")
            .old_text("one\ntwo\nthree\n"),
    )];
    let events = t.on_session_update(&SessionUpdate::ToolCall(call));

    let Some(ThreadEventItem::FileChange { changes, .. }) = events.iter().find_map(|e| match e {
        ProviderEvent::ItemStarted { item, .. } => Some(item),
        _ => None,
    }) else {
        panic!("expected a file change");
    };
    let diff = changes[0].diff.as_deref().expect("a patch");
    // Spelled the way the client's parser reads: a git header naming both
    // sides, the two header lines, then the hunks. An absolute path loses its
    // leading slash rather than becoming `a//srv/a.rs`.
    assert!(
        diff.starts_with("diff --git a/srv/a.rs b/srv/a.rs\n--- a/srv/a.rs\n+++ b/srv/a.rs\n@@"),
        "{diff}"
    );
    assert!(diff.contains("-two\n"), "{diff}");
    assert!(diff.contains("+two changed\n"), "{diff}");
    // Unchanged lines are context — one leading space — rather than a removal
    // and an addition of the same text.
    for context in [" one\n", " three\n"] {
        assert!(diff.contains(context), "{context:?} is missing from {diff}");
    }
    for unchanged in ["+one", "-one", "+three", "-three"] {
        assert!(
            !diff.contains(unchanged),
            "an unchanged line must not read as changed: {diff}"
        );
    }
}

/// pi writes its patch with `git diff --no-prefix`, which the client's parser
/// rejects outright: the row arrives with no file name and nothing drawn under
/// it. ACP v2 hands over that text as-is, so the adapter re-spells it.
#[test]
fn a_no_prefix_patch_is_respelled_into_the_canonical_form() {
    let mut t = translator();
    let patch = "diff --git main.rs main.rs\n\
                 --- main.rs\n\
                 +++ main.rs\n\
                 @@ -1,3 +1,3 @@\n\
                 \x20fn main() {\n\
                 -    println!(\"hello\");\n\
                 +    println!(\"loom\");\n\
                 \x20}\n";
    let update = v2::ToolCallUpdate::new("tool-1")
        .kind(v2::ToolKind::Edit)
        .status(v2::ToolCallStatus::Completed)
        .content(vec![v2::ToolCallContent::Diff(
            v2::Diff::new(vec![v2::DiffChange::modify("main.rs")])
                .with_patch(v2::DiffPatch::new(patch)),
        )]);
    let events = t.on_v2_session_update(&v2::SessionUpdate::ToolCallUpdate(update));

    let Some(ThreadEventItem::FileChange { changes, .. }) = events.iter().find_map(|e| match e {
        ProviderEvent::ItemStarted { item, .. } => Some(item),
        _ => None,
    }) else {
        panic!("expected a file change, got {events:?}");
    };
    assert_eq!(changes[0].path, "main.rs");
    assert_eq!(
        changes[0].diff.as_deref(),
        Some(
            "diff --git a/main.rs b/main.rs\n\
             --- a/main.rs\n\
             +++ b/main.rs\n\
             @@ -1,3 +1,3 @@\n\
             \x20fn main() {\n\
             -    println!(\"hello\");\n\
             +    println!(\"loom\");\n\
             \x20}\n"
        )
    );
}

/// A patch may cover several files. Each row carries its own file's hunks, not
/// the whole patch, since a row draws one file's diff.
#[test]
fn a_patch_covering_several_files_is_split_across_their_rows() {
    let mut t = translator();
    let patch = "diff --git a/a.rs b/a.rs\n\
                 --- a/a.rs\n\
                 +++ b/a.rs\n\
                 @@ -1 +1 @@\n\
                 -a old\n\
                 +a new\n\
                 diff --git a/b.rs b/b.rs\n\
                 --- a/b.rs\n\
                 +++ b/b.rs\n\
                 @@ -1 +1 @@\n\
                 -b old\n\
                 +b new\n";
    let update = v2::ToolCallUpdate::new("tool-1")
        .kind(v2::ToolKind::Edit)
        .status(v2::ToolCallStatus::Completed)
        .content(vec![v2::ToolCallContent::Diff(
            v2::Diff::new(vec![
                v2::DiffChange::modify("a.rs"),
                v2::DiffChange::modify("b.rs"),
            ])
            .with_patch(v2::DiffPatch::new(patch)),
        )]);
    let events = t.on_v2_session_update(&v2::SessionUpdate::ToolCallUpdate(update));

    let Some(ThreadEventItem::FileChange { changes, .. }) = events.iter().find_map(|e| match e {
        ProviderEvent::ItemStarted { item, .. } => Some(item),
        _ => None,
    }) else {
        panic!("expected a file change, got {events:?}");
    };
    assert_eq!(changes.len(), 2);
    let a = changes[0].diff.as_deref().expect("a's patch");
    assert!(a.starts_with("diff --git a/a.rs b/a.rs\n"), "{a}");
    assert!(a.contains("+a new\n"), "{a}");
    assert!(!a.contains("b new"), "a's row carries only a's hunks: {a}");
    let b = changes[1].diff.as_deref().expect("b's patch");
    assert!(b.starts_with("diff --git a/b.rs b/b.rs\n"), "{b}");
    assert!(b.contains("+b new\n"), "{b}");
    assert!(!b.contains("a new"), "b's row carries only b's hunks: {b}");
}

/// A patch with no hunks is not one a diff view can draw.
#[test]
fn a_patch_without_hunks_carries_no_diff() {
    let mut t = translator();
    let update = v2::ToolCallUpdate::new("tool-1")
        .kind(v2::ToolKind::Edit)
        .status(v2::ToolCallStatus::Completed)
        .content(vec![v2::ToolCallContent::Diff(
            v2::Diff::new(vec![v2::DiffChange::modify("/srv/a.rs")])
                .with_patch(v2::DiffPatch::new("diff --git a/a.rs b/a.rs\n")),
        )]);
    let events = t.on_v2_session_update(&v2::SessionUpdate::ToolCallUpdate(update));

    let Some(ThreadEventItem::FileChange { changes, .. }) = events.iter().find_map(|e| match e {
        ProviderEvent::ItemStarted { item, .. } => Some(item),
        _ => None,
    }) else {
        panic!("expected a file change, got {events:?}");
    };
    assert_eq!(changes[0].path, "/srv/a.rs");
    assert_eq!(changes[0].diff, None);
}

#[test]
fn a_deleted_file_carries_the_content_it_had() {
    let mut t = translator();
    let mut call = tool_call(ToolKind::Delete, None);
    call.content = vec![ToolCallContent::Diff(
        agent_client_protocol_schema::v1::Diff::new("/srv/old.rs", "").old_text("fn gone() {}\n"),
    )];
    let events = t.on_session_update(&SessionUpdate::ToolCall(call));

    let Some(ThreadEventItem::FileChange { changes, .. }) = events.iter().find_map(|e| match e {
        ProviderEvent::ItemStarted { item, .. } => Some(item),
        _ => None,
    }) else {
        panic!("expected a file change");
    };
    assert!(matches!(
        changes[0].kind,
        loom_domain::FileChangeKind::Delete
    ));
    assert_eq!(changes[0].diff.as_deref(), Some("fn gone() {}\n"));
}

#[test]
fn a_multi_file_edit_is_one_item_with_several_changes() {
    // ACP's content is an array, which the contract can express and the Pi
    // path cannot.
    let mut t = translator();
    let mut call = tool_call(ToolKind::Edit, None);
    call.content = vec![
        ToolCallContent::Diff(
            agent_client_protocol_schema::v1::Diff::new("/srv/a.rs", "a").old_text("old"),
        ),
        ToolCallContent::Diff(agent_client_protocol_schema::v1::Diff::new(
            "/srv/b.rs",
            "b",
        )),
    ];
    let events = t.on_session_update(&SessionUpdate::ToolCall(call));

    match events.iter().find_map(|e| match e {
        ProviderEvent::ItemStarted { item, .. } => Some(item),
        _ => None,
    }) {
        Some(ThreadEventItem::FileChange { changes, .. }) => {
            assert_eq!(changes.len(), 2);
            assert!(matches!(
                changes[0].kind,
                loom_domain::FileChangeKind::Update
            ));
            // No `old_text` means the file did not exist before.
            assert!(matches!(changes[1].kind, loom_domain::FileChangeKind::Add));
        }
        other => panic!("expected a file change, got {other:?}"),
    }
}

// --- tool lifecycle --------------------------------------------------------

#[test]
fn a_tool_call_opens_once_and_completes_once_with_the_merged_shape() {
    let mut t = translator();
    let call = tool_call(
        ToolKind::Execute,
        Some(serde_json::json!({"command": "ls"})),
    );
    let opened = t.on_session_update(&SessionUpdate::ToolCall(call));

    let starts = opened
        .iter()
        .filter(|e| matches!(e, ProviderEvent::ItemStarted { .. }))
        .count();
    assert_eq!(starts, 1, "one start per call: {opened:?}");

    // An in-progress patch reports progress and does not complete the item.
    let progress = t.on_session_update(&SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
        "call-1",
        ToolCallUpdateFields::new().status(Some(ToolCallStatus::InProgress)),
    )));
    assert!(
        !progress
            .iter()
            .any(|e| matches!(e, ProviderEvent::ItemCompleted { .. })),
        "an in-progress patch must not complete the item: {progress:?}"
    );

    let done = t.on_session_update(&SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
        "call-1",
        ToolCallUpdateFields::new().status(Some(ToolCallStatus::Completed)),
    )));
    let completions: Vec<_> = done
        .iter()
        .filter_map(|e| match e {
            ProviderEvent::ItemCompleted { item, .. } => Some(item),
            _ => None,
        })
        .collect();
    assert_eq!(completions.len(), 1, "exactly one completion: {done:?}");
    match completions[0] {
        ThreadEventItem::CommandExecution {
            command, status, ..
        } => {
            assert_eq!(command, "ls", "the merged item keeps what the start knew");
            assert_eq!(*status, ItemStatus::Completed, "the status moved");
        }
        other => panic!("expected the accumulated item, got {other:?}"),
    }
}

#[test]
fn a_completed_tool_call_cannot_be_reopened() {
    let mut t = translator();
    let call = tool_call(
        ToolKind::Execute,
        Some(serde_json::json!({"command": "ls"})),
    );
    t.on_session_update(&SessionUpdate::ToolCall(call));
    t.on_session_update(&SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
        "call-1",
        ToolCallUpdateFields::new().status(Some(ToolCallStatus::Completed)),
    )));

    let late = t.on_session_update(&SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
        "call-1",
        ToolCallUpdateFields::new().status(Some(ToolCallStatus::InProgress)),
    )));
    assert!(
        late.is_empty(),
        "a finished call is forgotten, so a late patch does nothing: {late:?}"
    );
}

#[test]
fn an_update_for_an_unknown_call_is_ignored() {
    let mut t = translator();
    let events = t.on_session_update(&SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
        "never-seen",
        ToolCallUpdateFields::new().status(Some(ToolCallStatus::Completed)),
    )));
    assert!(
        events.is_empty(),
        "loom does not fabricate a start it never saw: {events:?}"
    );
}

#[test]
fn a_tool_call_closes_the_streaming_assistant_message() {
    let mut t = translator();
    t.on_session_update(&SessionUpdate::AgentMessageChunk(text_chunk(
        "thinking aloud",
    )));
    let events = t.on_session_update(&SessionUpdate::ToolCall(tool_call(
        ToolKind::Execute,
        Some(serde_json::json!({"command": "ls"})),
    )));

    // The model stopped to call something, so the message it was writing ends.
    assert!(
        events.iter().any(|e| matches!(
            e,
            ProviderEvent::ItemCompleted {
                item: ThreadEventItem::AgentMessage { .. },
                ..
            }
        )),
        "the assistant message is flushed before the tool item: {events:?}"
    );
}

// --- item identity ---------------------------------------------------------

#[test]
fn consecutive_chunks_share_one_item_id() {
    let mut t = translator();
    let first = t.on_session_update(&SessionUpdate::AgentMessageChunk(text_chunk("a")));
    let second = t.on_session_update(&SessionUpdate::AgentMessageChunk(text_chunk("b")));

    assert_eq!(
        delta_id(&first[0]),
        delta_id(&second[0]),
        "chunks of one message must group under one id"
    );
}

#[test]
fn a_new_message_after_a_flush_gets_a_new_id() {
    let mut t = translator();
    let first = t.on_session_update(&SessionUpdate::AgentMessageChunk(text_chunk("a")));
    t.on_stop_reason(StopReason::EndTurn);
    let second = t.on_session_update(&SessionUpdate::AgentMessageChunk(text_chunk("b")));

    assert_ne!(
        delta_id(&first[0]),
        delta_id(&second[0]),
        "a flushed message cannot be extended by a later turn"
    );
}

#[test]
fn identity_is_emitted_once_per_run() {
    let mut t = translator();
    let first = t.on_prompt_sent();
    assert_eq!(
        first
            .iter()
            .filter(|e| matches!(e, ProviderEvent::ThreadIdentity { .. }))
            .count(),
        1
    );
    let second = t.on_prompt_sent();
    assert_eq!(
        second
            .iter()
            .filter(|e| matches!(e, ProviderEvent::ThreadIdentity { .. }))
            .count(),
        0,
        "identity is a thread fact, stated once"
    );
    assert_eq!(
        second
            .iter()
            .filter(|e| matches!(e, ProviderEvent::TurnStarted { .. }))
            .count(),
        0,
        "the second prompt does not reopen an open turn"
    );
}

// --- updates that carry no timeline fact -----------------------------------

#[test]
fn non_timeline_updates_produce_no_events() {
    use agent_client_protocol_schema::v1::{
        AvailableCommandsUpdate, ConfigOptionUpdate, CurrentModeUpdate,
    };
    let mut t = translator();

    for update in [
        SessionUpdate::CurrentModeUpdate(CurrentModeUpdate::new("code")),
        SessionUpdate::ConfigOptionUpdate(ConfigOptionUpdate::new(vec![])),
        SessionUpdate::AvailableCommandsUpdate(AvailableCommandsUpdate::new(vec![])),
    ] {
        let events = t.on_session_update(&update);
        assert!(
            events.is_empty(),
            "{update:?} states no timeline fact, so it emits nothing"
        );
    }
}

#[test]
fn a_session_title_becomes_a_name_update() {
    let mut t = translator();
    let events = t.on_session_update(&SessionUpdate::SessionInfoUpdate(
        SessionInfoUpdate::new().title("Fix the build"),
    ));

    match events.first() {
        Some(ProviderEvent::ThreadNameUpdated { thread_name, .. }) => {
            assert_eq!(thread_name, "Fix the build");
        }
        other => panic!("expected a name update, got {other:?}"),
    }
}

#[test]
fn an_untitled_session_info_produces_nothing() {
    let mut t = translator();
    // `title` is a patch field: undefined means "unchanged", which is not a
    // name to report.
    let events = t.on_session_update(&SessionUpdate::SessionInfoUpdate(SessionInfoUpdate::new()));
    assert!(events.is_empty(), "got {events:?}");
}

// --- plan ------------------------------------------------------------------

#[test]
fn a_plan_becomes_the_contract_step_list() {
    let mut t = translator();
    let plan = Plan::new(vec![
        PlanEntry::new(
            "read the file",
            PlanEntryPriority::High,
            PlanEntryStatus::Completed,
        ),
        PlanEntry::new(
            "edit it",
            PlanEntryPriority::Medium,
            PlanEntryStatus::InProgress,
        ),
        PlanEntry::new(
            "run tests",
            PlanEntryPriority::Low,
            PlanEntryStatus::Pending,
        ),
    ]);
    let events = t.on_session_update(&SessionUpdate::Plan(plan));

    match events.first() {
        Some(ProviderEvent::TurnPlanUpdated { plan, .. }) => {
            assert_eq!(plan.len(), 3);
            assert!(matches!(
                plan[0].status,
                Some(loom_domain::PlanStepStatus::Completed)
            ));
            assert!(
                matches!(plan[1].status, Some(loom_domain::PlanStepStatus::Active)),
                "ACP's `InProgress` is the contract's `Active`"
            );
        }
        other => panic!("expected a plan update, got {other:?}"),
    }
}

#[test]
fn a_v2_full_message_patch_only_emits_the_new_suffix() {
    let mut t = translator();
    t.on_prompt_sent();

    let chunk = v2::ContentChunk::new(
        v2::ContentBlock::Text(v2::TextContent::new("hello")),
        "message-1",
    );
    let first = t.on_v2_session_update(&v2::SessionUpdate::AgentMessageChunk(chunk));
    assert!(matches!(
        first.first(),
        Some(ProviderEvent::ItemAgentMessageDelta { delta, .. }) if delta == "hello"
    ));

    let patch = v2::AgentMessage::new("message-1").content(vec![v2::ContentBlock::Text(
        v2::TextContent::new("hello world"),
    )]);
    let suffix = t.on_v2_session_update(&v2::SessionUpdate::AgentMessage(patch.clone()));
    assert!(matches!(
        suffix.first(),
        Some(ProviderEvent::ItemAgentMessageDelta { delta, .. }) if delta == " world"
    ));
    assert!(
        t.on_v2_session_update(&v2::SessionUpdate::AgentMessage(patch))
            .is_empty(),
        "a repeated authoritative patch must not duplicate persisted text"
    );

    let done = t.on_v2_session_update(&v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(
        v2::IdleStateUpdate::new().stop_reason(v2::StopReason::EndTurn),
    )));
    assert_eq!(
        done.iter()
            .filter(|event| matches!(event, ProviderEvent::TurnCompleted { .. }))
            .count(),
        1
    );
}

#[test]
fn a_v2_tool_upsert_maps_to_the_existing_tool_lifecycle() {
    let mut t = translator();
    let started = v2::ToolCallUpdate::new("tool-1")
        .kind(v2::ToolKind::Execute)
        .status(v2::ToolCallStatus::InProgress)
        .raw_input(serde_json::json!({"command": "cargo test"}));
    let events = t.on_v2_session_update(&v2::SessionUpdate::ToolCallUpdate(started));
    assert!(matches!(
        events.first(),
        Some(ProviderEvent::ItemStarted {
            item: ThreadEventItem::CommandExecution { command, .. },
            ..
        }) if command == "cargo test"
    ));

    let done = v2::ToolCallUpdate::new("tool-1").status(v2::ToolCallStatus::Completed);
    let events = t.on_v2_session_update(&v2::SessionUpdate::ToolCallUpdate(done));
    assert!(matches!(
        events.last(),
        Some(ProviderEvent::ItemCompleted {
            item: ThreadEventItem::CommandExecution { status, .. },
            ..
        }) if *status == ItemStatus::Completed
    ));
}

#[test]
fn a_v2_execute_call_takes_its_command_from_the_title() {
    // pi-acp sends no raw input: the command is the call's title. Reading only
    // `raw_input.command` left the call a tool named "execute" with no
    // arguments, which is what a `pwd` run showed as.
    let mut t = translator();
    let started = v2::ToolCallUpdate::new("tool-1")
        .title("pwd")
        .kind(v2::ToolKind::Execute)
        .status(v2::ToolCallStatus::Pending);
    let events = t.on_v2_session_update(&v2::SessionUpdate::ToolCallUpdate(started));
    assert!(matches!(
        events.first(),
        Some(ProviderEvent::ItemStarted {
            item: ThreadEventItem::CommandExecution { command, status, aggregated_output, .. },
            ..
        }) if command == "pwd"
            && *status == ItemStatus::Pending
            && aggregated_output.is_none()
    ));
}

/// pi-acp streams the terminal through `_meta`: one `terminal_output` chunk per
/// piece of output and a `terminal_exit` at the end. Without reading them the
/// command row carried an empty output and no exit code.
#[test]
fn a_v2_execute_call_reads_its_output_from_the_terminal_meta() {
    let mut t = translator();
    t.on_v2_session_update(&v2::SessionUpdate::ToolCallUpdate(
        v2::ToolCallUpdate::new("tool-1")
            .title("printf hello")
            .kind(v2::ToolKind::Execute)
            .status(v2::ToolCallStatus::Pending),
    ));

    let chunk = v2::ToolCallUpdate::new("tool-1")
        .status(v2::ToolCallStatus::InProgress)
        .meta(terminal_meta(&[(
            "terminal_output",
            serde_json::json!({"terminal_id": "tool-1", "data": "hello\n"}),
        )]));
    t.on_v2_session_update(&v2::SessionUpdate::ToolCallUpdate(chunk));
    let chunk = v2::ToolCallUpdate::new("tool-1")
        .status(v2::ToolCallStatus::InProgress)
        .meta(terminal_meta(&[(
            "terminal_output",
            serde_json::json!({"terminal_id": "tool-1", "data": "world\n"}),
        )]));
    t.on_v2_session_update(&v2::SessionUpdate::ToolCallUpdate(chunk));

    let done = v2::ToolCallUpdate::new("tool-1")
        .status(v2::ToolCallStatus::Completed)
        .meta(terminal_meta(&[
            (
                "terminal_output",
                serde_json::json!({"terminal_id": "tool-1", "data": "done\n"}),
            ),
            (
                "terminal_exit",
                serde_json::json!({"terminal_id": "tool-1", "exit_code": 0, "signal": null}),
            ),
        ]));
    let events = t.on_v2_session_update(&v2::SessionUpdate::ToolCallUpdate(done));
    let Some(ProviderEvent::ItemCompleted { item, .. }) = events
        .iter()
        .find(|event| matches!(event, ProviderEvent::ItemCompleted { .. }))
    else {
        panic!("expected a completed item, got {events:?}");
    };
    let ThreadEventItem::CommandExecution {
        command,
        aggregated_output,
        exit_code,
        status,
        ..
    } = item
    else {
        panic!("expected a command execution, got {item:?}");
    };
    assert_eq!(command, "printf hello");
    // Chunks accumulate in arrival order.
    assert_eq!(aggregated_output.as_deref(), Some("hello\nworld\ndone\n"));
    assert_eq!(*exit_code, Some(0));
    assert_eq!(*status, ItemStatus::Completed);
}

/// The output is only the terminal's: an entry naming another terminal must not
/// land on this call's row.
#[test]
fn a_v2_execute_call_ignores_another_terminals_meta() {
    let mut t = translator();
    t.on_v2_session_update(&v2::SessionUpdate::ToolCallUpdate(
        v2::ToolCallUpdate::new("tool-1")
            .title("pwd")
            .kind(v2::ToolKind::Execute)
            .status(v2::ToolCallStatus::Pending),
    ));
    let stale = v2::ToolCallUpdate::new("tool-1")
        .status(v2::ToolCallStatus::Completed)
        .meta(terminal_meta(&[
            (
                "terminal_output",
                serde_json::json!({"terminal_id": "tool-other", "data": "not mine\n"}),
            ),
            (
                "terminal_exit",
                serde_json::json!({"terminal_id": "tool-other", "exit_code": 9}),
            ),
        ]));
    let events = t.on_v2_session_update(&v2::SessionUpdate::ToolCallUpdate(stale));
    let Some(ProviderEvent::ItemCompleted { item, .. }) = events
        .iter()
        .find(|event| matches!(event, ProviderEvent::ItemCompleted { .. }))
    else {
        panic!("expected a completed item, got {events:?}");
    };
    let ThreadEventItem::CommandExecution {
        aggregated_output,
        exit_code,
        ..
    } = item
    else {
        panic!("expected a command execution, got {item:?}");
    };
    assert_eq!(*aggregated_output, None);
    assert_eq!(*exit_code, None);
}

/// A title on a call that is not an execute is not a command.
#[test]
fn a_v2_title_on_another_kind_stays_a_tool() {
    let mut t = translator();
    let call = v2::ToolCallUpdate::new("tool-1")
        .title("Fetching the changelog")
        .kind(v2::ToolKind::Other)
        .status(v2::ToolCallStatus::Pending);
    let events = t.on_v2_session_update(&v2::SessionUpdate::ToolCallUpdate(call));
    assert!(matches!(
        events.first(),
        Some(ProviderEvent::ItemStarted {
            item: ThreadEventItem::ToolCall { .. },
            ..
        })
    ));
}

/// The generic tool call a v2 update opened: its name and its result.
fn started_tool_call(events: &[ProviderEvent]) -> (String, Option<serde_json::Value>) {
    let item = events.iter().find_map(|event| match event {
        ProviderEvent::ItemStarted { item, .. } => Some(item),
        _ => None,
    });
    let Some(ThreadEventItem::ToolCall { tool, result, .. }) = item else {
        panic!("expected a tool call, got {events:?}");
    };
    (tool.clone(), result.clone())
}

/// A generic call is named by the call's own title, not by its kind.
///
/// pi files every tool that is not read, write, edit or bash under
/// `ToolKind::Other` and spells the pi tool name in `title`, so a row that read
/// the kind showed `"other"` for a call whose name the agent had sent.
#[test]
fn a_v2_generic_call_is_named_by_its_title() {
    let mut t = translator();
    let call = v2::ToolCallUpdate::new("tool-1")
        .title("cymbal")
        .kind(v2::ToolKind::Other)
        .status(v2::ToolCallStatus::Completed);
    let events = t.on_v2_session_update(&v2::SessionUpdate::ToolCallUpdate(call));
    assert_eq!(started_tool_call(&events).0, "cymbal");
}

/// A kind that carries its own name is the protocol's word, not the agent's prose.
#[test]
fn a_v2_unknown_kind_name_wins_over_the_title() {
    let mut t = translator();
    let call = v2::ToolCallUpdate::new("tool-1")
        .title("Reviewing the diff")
        .kind(v2::ToolKind::Unknown("review".to_owned()))
        .status(v2::ToolCallStatus::Completed);
    let events = t.on_v2_session_update(&v2::SessionUpdate::ToolCallUpdate(call));
    assert_eq!(started_tool_call(&events).0, "review");
}

/// A call with no name anywhere keeps the kind's own word.
#[test]
fn a_v2_generic_call_without_a_title_stays_other() {
    let mut t = translator();
    let call = v2::ToolCallUpdate::new("tool-1").status(v2::ToolCallStatus::Completed);
    let events = t.on_v2_session_update(&v2::SessionUpdate::ToolCallUpdate(call));
    assert_eq!(started_tool_call(&events).0, "other");
}

/// The text ACP carried as the call's content is the row's result.
///
/// pi renders the tool result's text into `content` and leaves the agent's own
/// value in `raw_output`; taking the raw value showed the whole envelope where
/// the tool's answer belongs.
#[test]
fn a_v2_generic_call_reports_its_content_as_the_result() {
    let mut t = translator();
    let call = v2::ToolCallUpdate::new("tool-1")
        .title("cymbal")
        .kind(v2::ToolKind::Other)
        .status(v2::ToolCallStatus::Completed)
        .content(vec![v2::ToolCallContent::Content(Box::new(
            v2::Content::new(v2::ContentBlock::Text(v2::TextContent::new(
                "repo: loom\nfiles: 1942\n",
            ))),
        ))])
        .raw_output(serde_json::json!({
            "content": [{"type": "text", "text": "repo: loom\nfiles: 1942\n"}],
            "details": {"exitCode": 0},
        }));
    let events = t.on_v2_session_update(&v2::SessionUpdate::ToolCallUpdate(call));
    assert_eq!(
        started_tool_call(&events).1,
        Some(serde_json::json!("repo: loom\nfiles: 1942\n"))
    );
}

/// Without content blocks, an MCP-style envelope is unwrapped to its text.
#[test]
fn a_v2_generic_call_unwraps_a_content_envelope() {
    let mut t = translator();
    let call = v2::ToolCallUpdate::new("tool-1")
        .title("cymbal")
        .kind(v2::ToolKind::Other)
        .status(v2::ToolCallStatus::Completed)
        .raw_output(serde_json::json!({
            "content": [{"type": "text", "text": "hello"}],
            "details": {"exitCode": 0},
        }));
    let events = t.on_v2_session_update(&v2::SessionUpdate::ToolCallUpdate(call));
    assert_eq!(
        started_tool_call(&events).1,
        Some(serde_json::json!("hello"))
    );
}

/// A value loom cannot read is still shown rather than dropped.
#[test]
fn a_v2_generic_call_keeps_a_raw_output_it_cannot_read() {
    let mut t = translator();
    let call = v2::ToolCallUpdate::new("tool-1")
        .title("cymbal")
        .kind(v2::ToolKind::Other)
        .status(v2::ToolCallStatus::Completed)
        .raw_output(serde_json::json!({"count": 3}));
    let events = t.on_v2_session_update(&v2::SessionUpdate::ToolCallUpdate(call));
    assert_eq!(
        started_tool_call(&events).1,
        Some(serde_json::json!({"count": 3}))
    );
}

fn terminal_meta(entries: &[(&str, serde_json::Value)]) -> v2::Meta {
    entries
        .iter()
        .map(|(key, value)| ((*key).to_owned(), value.clone()))
        .collect()
}

#[test]
fn a_v2_terminal_decodes_output_and_completes_once() {
    use base64::Engine as _;

    let mut t = translator();
    let initial_output = base64::engine::general_purpose::STANDARD.encode("old");
    let started = v2::TerminalUpdate::new("terminal-1")
        .command("printf old")
        .output(v2::TerminalOutput::new(initial_output));
    let events = t.on_v2_session_update(&v2::SessionUpdate::TerminalUpdate(started));
    assert!(matches!(
        events.first(),
        Some(ProviderEvent::ItemStarted {
            item: ThreadEventItem::CommandExecution {
                aggregated_output: Some(output),
                ..
            },
            ..
        }) if output == "old"
    ));

    let chunk = base64::engine::general_purpose::STANDARD.encode(" next");
    let events = t.on_v2_session_update(&v2::SessionUpdate::TerminalOutputChunk(
        v2::TerminalOutputChunk::new("terminal-1", chunk),
    ));
    assert!(matches!(
        events.last(),
        Some(ProviderEvent::ItemCommandExecutionOutputDelta { item_id, delta, .. })
            if item_id == "terminal-v2-terminal-1" && delta == " next"
    ));

    let exited = v2::TerminalUpdate::new("terminal-1")
        .exit_status(v2::TerminalExitStatus::new().exit_code(0));
    let events = t.on_v2_session_update(&v2::SessionUpdate::TerminalUpdate(exited));
    assert!(matches!(
        events.last(),
        Some(ProviderEvent::ItemCompleted {
            item: ThreadEventItem::CommandExecution {
                status,
                aggregated_output: Some(output),
                exit_code: Some(0),
                ..
            },
            ..
        }) if *status == ItemStatus::Completed && output == "old next"
    ));
}

#[test]
fn v2_plan_usage_name_and_unknown_updates_use_the_v1_contract() {
    let mut t = translator();
    let plan = v2::PlanUpdate::new(v2::PlanUpdateContent::items(
        "plan-1",
        vec![v2::PlanEntry::new(
            "ship it",
            v2::PlanEntryPriority::High,
            v2::PlanEntryStatus::InProgress,
        )],
    ));
    assert!(matches!(
        t.on_v2_session_update(&v2::SessionUpdate::PlanUpdate(plan))
            .first(),
        Some(ProviderEvent::TurnPlanUpdated { plan, .. })
            if plan[0].status == Some(loom_domain::PlanStepStatus::Active)
    ));

    assert!(matches!(
        t.on_v2_session_update(&v2::SessionUpdate::UsageUpdate(v2::UsageUpdate::new(
            4, 100
        ),))
            .first(),
        Some(ProviderEvent::ThreadContextWindowUsageUpdated { .. })
    ));
    assert!(matches!(
        t.on_v2_session_update(&v2::SessionUpdate::SessionInfoUpdate(
            v2::SessionInfoUpdate::new().title("v2 session"),
        ))
        .first(),
        Some(ProviderEvent::ThreadNameUpdated { thread_name, .. })
            if thread_name == "v2 session"
    ));

    let unknown = v2::OtherSessionUpdate::new("future_update", std::collections::BTreeMap::new());
    assert!(t
        .on_v2_session_update(&v2::SessionUpdate::Other(unknown))
        .is_empty());
}
