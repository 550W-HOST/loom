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
use loom_domain::{ItemStatus, ProviderEvent, ThreadEventItem, TurnStatus};

use super::{AcpTranslator, RunContext};

fn translator() -> AcpTranslator {
    AcpTranslator::new(RunContext {
        thread_id: loom_domain::ThreadId::mint(),
        cwd: Some("/srv/project".into()),
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
