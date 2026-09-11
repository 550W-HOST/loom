//! Conformance between loom's event model and bb's exported `ThreadEvent`
//! contract.
//!
//! The acceptance criterion this file exists for is: **the frame a consumer
//! receives validates against `contracts/bb/thread-event.json`**. A Rust type
//! that "looks right" is not enough — the discriminant and every field name
//! must match, because the projection layer dispatches on `event.type` and
//! reads camelCase fields.
//!
//! Two layers are checked here:
//!
//! 1. **The type set.** Every one of the contract's 48 types is classified by
//!    [`loom_domain::ThreadEventType`]: 35 provider types and 13 client/system
//!    types. Nothing is left unclassified, and nothing is invented.
//! 2. **The wire shape.** A representative Rust value for each of the 35
//!    provider types round-trips through serde, is wrapped in a
//!    [`loom_domain::RunEvent`], and validates against the contract.
//!
//! The inner `event` payload is what is validated: the loom envelope
//! (`thread_id`, `project_id`, `run_id`, `at_ms`) is loom's, and the contract
//! only ever sees the [`loom_domain::ThreadEvent`].

use loom_contract::Contract;
use loom_domain::{
    PlanStep, PlanStepStatus, ProviderEvent, ProviderEventType, RunEvent, ThreadEventItem,
    ThreadEventType, TurnStatus,
};
use serde_json::json;

fn ids() -> (
    loom_domain::ThreadId,
    loom_domain::ProjectId,
    loom_domain::RunId,
) {
    (
        loom_domain::ThreadId::mint(),
        loom_domain::ProjectId::mint(),
        loom_domain::RunId::mint(),
    )
}

/// One representative value for every provider event type.
///
/// The list is exhaustive over [`ProviderEventType::ALL`]: a new contract type
/// without a sample fails the coverage test below rather than being silently
/// omitted.
fn samples() -> Vec<ProviderEvent> {
    let ptid = "thr_session".to_string();
    vec![
        ProviderEvent::ThreadStarted {},
        ProviderEvent::ThreadIdentity {
            provider_thread_id: ptid.clone(),
        },
        ProviderEvent::TurnStarted {
            provider_thread_id: ptid.clone(),
            parent_tool_call_id: None,
        },
        ProviderEvent::TurnCompleted {
            provider_thread_id: Some(ptid.clone()),
            status: TurnStatus::Completed,
            error: None,
            provider_checkpoint_id: Some("checkpoint-1".into()),
        },
        ProviderEvent::TurnInputAccepted {
            client_request_id: "creq_abcdefghij".into(),
            provider_thread_id: ptid.clone(),
        },
        ProviderEvent::ThreadNameUpdated {
            provider_thread_id: ptid.clone(),
            thread_name: "a name".into(),
        },
        ProviderEvent::ThreadCompacted {
            provider_thread_id: ptid.clone(),
        },
        ProviderEvent::ThreadContextCleared {
            provider_thread_id: ptid.clone(),
        },
        ProviderEvent::ThreadGoalUpdated {
            provider_thread_id: ptid.clone(),
            objective: "ship it".into(),
            status: loom_domain::GoalStatus::Active,
            time_used_seconds: 1.0,
            token_budget: Some(1000.0),
            tokens_used: 10.0,
        },
        ProviderEvent::ThreadGoalCleared {
            provider_thread_id: ptid.clone(),
        },
        ProviderEvent::ItemStarted {
            item: ThreadEventItem::AgentMessage {
                id: "assistant-1".into(),
                text: "hello".into(),
                presentation: None,
                parent_tool_call_id: None,
            },
            provider_thread_id: ptid.clone(),
        },
        ProviderEvent::ItemCompleted {
            item: ThreadEventItem::CommandExecution {
                id: "tool-1".into(),
                command: "ls".into(),
                cwd: "/srv".into(),
                status: loom_domain::ItemStatus::Completed,
                approval_status: None,
                aggregated_output: Some("a\nb".into()),
                exit_code: Some(0),
                duration_ms: Some(3.0),
                presentation: None,
                parent_tool_call_id: None,
            },
            provider_thread_id: ptid.clone(),
        },
        ProviderEvent::ItemAgentMessageDelta {
            item_id: "assistant-1".into(),
            delta: "hello".into(),
            provider_thread_id: ptid.clone(),
            parent_tool_call_id: None,
        },
        ProviderEvent::ItemCommandExecutionOutputDelta {
            item_id: "tool-1".into(),
            delta: "a\n".into(),
            provider_thread_id: ptid.clone(),
            reset: Some(true),
            parent_tool_call_id: None,
        },
        ProviderEvent::ItemFileChangeOutputDelta {
            item_id: "tool-2".into(),
            delta: "diff".into(),
            provider_thread_id: ptid.clone(),
            parent_tool_call_id: None,
        },
        ProviderEvent::ItemReasoningSummaryTextDelta {
            item_id: "reasoning-1".into(),
            delta: "summary".into(),
            provider_thread_id: ptid.clone(),
            parent_tool_call_id: None,
        },
        ProviderEvent::ItemReasoningTextDelta {
            item_id: "reasoning-1".into(),
            delta: "thought".into(),
            provider_thread_id: ptid.clone(),
            parent_tool_call_id: None,
        },
        ProviderEvent::ItemPlanDelta {
            item_id: "plan-1".into(),
            delta: "step".into(),
            provider_thread_id: ptid.clone(),
            parent_tool_call_id: None,
        },
        ProviderEvent::ItemMcpToolCallProgress {
            item_id: "mcp-1".into(),
            message: Some("working".into()),
            provider_thread_id: ptid.clone(),
            parent_tool_call_id: None,
        },
        ProviderEvent::ItemToolCallProgress {
            item_id: "tool-3".into(),
            message: Some("working".into()),
            provider_thread_id: ptid.clone(),
            parent_tool_call_id: None,
        },
        ProviderEvent::ItemBackgroundTaskProgress {
            item: background_task(),
            provider_thread_id: ptid.clone(),
        },
        ProviderEvent::ItemBackgroundTaskCompleted {
            item: background_task(),
            provider_thread_id: ptid.clone(),
        },
        ProviderEvent::ItemDelegationProgress {
            item: delegation(),
            provider_thread_id: ptid.clone(),
        },
        ProviderEvent::ItemDelegationCompleted {
            item: delegation(),
            provider_thread_id: ptid.clone(),
        },
        ProviderEvent::ThreadTokenUsageUpdated {
            provider_thread_id: ptid.clone(),
            token_usage: loom_domain::ThreadTokenUsage {
                total: breakdown(),
                last: breakdown(),
                model_context_window: Some(200_000),
            },
        },
        ProviderEvent::ThreadContextWindowUsageUpdated {
            provider_thread_id: ptid.clone(),
            context_window_usage: loom_domain::ContextWindowUsage {
                used_tokens: Some(100),
                model_context_window: Some(200_000),
                estimated: false,
            },
        },
        ProviderEvent::TurnPlanUpdated {
            provider_thread_id: ptid.clone(),
            plan: vec![PlanStep {
                step: "do it".into(),
                status: Some(PlanStepStatus::Active),
            }],
            explanation: Some("because".into()),
        },
        ProviderEvent::TurnDiffUpdated {
            provider_thread_id: ptid.clone(),
            diff: Some("--- a\n+++ b\n".into()),
        },
        ProviderEvent::ProviderError {
            provider_thread_id: ptid.clone(),
            message: "overloaded".into(),
            detail: Some("529".into()),
            error_info: Some(loom_domain::ProviderErrorInfo {
                category: loom_domain::ProviderErrorCategory::Overloaded,
                provider_code: Some("overloaded_error".into()),
                http_status_code: Some(529),
            }),
            will_retry: Some(true),
        },
        ProviderEvent::ProviderRateLimitsUpdated {
            provider_thread_id: ptid.clone(),
            rate_limits: json!({
                "providerId": "pi",
                "status": "allowed",
                "kind": "subscription-window",
                "windows": [],
                "reachedReason": null,
                "overageStatus": null,
                "overageReason": null,
            }),
        },
        ProviderEvent::ProviderEnvResolved {
            provider_thread_id: ptid.clone(),
            entries: vec![loom_domain::EnvResolvedEntry {
                name: "ANTHROPIC_API_KEY".into(),
                source: loom_domain::EnvResolvedSource::Shell,
                value: loom_domain::EnvResolvedValue::Masked { masked: true },
                reason: Some("from the shell".into()),
            }],
        },
        ProviderEvent::ThreadExtensionStateUpdated {
            provider_thread_id: ptid.clone(),
            kind: "provider-pi/state".into(),
            payload: json!({ "note": "x" }),
        },
        ProviderEvent::ProviderWarning {
            provider_thread_id: ptid.clone(),
            category: loom_domain::ProviderWarningCategory::Config,
            summary: Some("check config".into()),
            details: Some("detail".into()),
        },
        ProviderEvent::ProviderModelFallback {
            provider_thread_id: ptid.clone(),
            original_model: "a".into(),
            fallback_model: "b".into(),
            reason: loom_domain::ModelFallbackReason::Provider,
            message: "fell back".into(),
        },
        ProviderEvent::ProviderUnhandled {
            provider_thread_id: ptid.clone(),
            provider_id: "pi".into(),
            raw_type: "bash_execution_update".into(),
            raw_event: loom_domain::ProviderRawEvent {
                jsonrpc: "2.0".into(),
                id: Some(json!("req-1")),
                method: "bash_execution_update".into(),
                params: None,
            },
            parent_tool_call_id: None,
        },
    ]
}

fn breakdown() -> loom_domain::TokenUsageBreakdown {
    loom_domain::TokenUsageBreakdown {
        total_tokens: 14,
        input_tokens: 10,
        cached_input_tokens: 2,
        output_tokens: 2,
        reasoning_output_tokens: 0,
    }
}

fn background_task() -> ThreadEventItem {
    ThreadEventItem::BackgroundTask {
        id: "task-1".into(),
        family_id: None,
        task_type: "workflow".into(),
        description: "a task".into(),
        status: loom_domain::ItemStatus::Pending,
        task_status: "running".into(),
        skip_transcript: false,
        workflow_name: None,
        workflow: None,
        usage: None,
        summary: None,
        error: None,
        output_file: None,
        presentation: None,
        parent_tool_call_id: None,
    }
}

fn delegation() -> ThreadEventItem {
    ThreadEventItem::Delegation {
        id: "delegation-1".into(),
        child_ref: "thr_child".into(),
        label: "child".into(),
        status: loom_domain::ItemStatus::Pending,
        background: true,
        summary: None,
        presentation: None,
        parent_tool_call_id: None,
    }
}

#[test]
fn every_contract_type_is_classified() {
    let contract = Contract::load();
    let declared = contract.thread_event_types();
    assert_eq!(declared.len(), 48, "the contract's type count moved");

    for event_type in &declared {
        let classified = ThreadEventType::parse(event_type)
            .unwrap_or_else(|error| panic!("`{event_type}` is unclassified: {error}"));
        assert_eq!(&classified.as_str(), event_type);
        // Asking for the provider body must be the only thing that rejects a
        // client/system type — the classification itself is total.
        if classified.is_provider_event() {
            assert!(classified.provider().is_ok());
        } else {
            assert!(matches!(
                classified.provider(),
                Err(loom_domain::ProviderEventError::NotAPProviderEvent(_))
            ));
        }
    }

    // And loom's own enums declare exactly the contract's set, in both
    // directions: nothing missing, nothing invented.
    let loom_types: Vec<&str> = ThreadEventType::ALL.iter().map(|t| t.as_str()).collect();
    assert_eq!(loom_types.len(), declared.len());
    for event_type in &declared {
        assert!(
            loom_types.contains(event_type),
            "`{event_type}` is in the contract but not in loom's type set"
        );
    }
    let provider_types: Vec<&str> = ProviderEventType::ALL.iter().map(|t| t.as_str()).collect();
    assert_eq!(
        provider_types.len(),
        35,
        "the contract declares 35 provider event types"
    );
}

#[test]
fn every_provider_type_has_a_sample() {
    let samples = samples();
    let produced: Vec<&str> = samples.iter().map(ProviderEvent::kind).collect();
    for event_type in ProviderEventType::ALL {
        assert!(
            produced.contains(&event_type.as_str()),
            "`{}` has no sample; add one so the wire shape is checked",
            event_type.as_str()
        );
    }
    assert_eq!(samples.len(), 35);
}

#[test]
fn every_provider_event_validates_against_the_contract() {
    let contract = Contract::load();
    for event in samples() {
        let (thread_id, project_id, run_id) = ids();
        let kind = event.kind();
        let run_event = RunEvent::new(thread_id, project_id, run_id, 1, event);
        let value = serde_json::to_value(&run_event).expect("a run event serializes");

        // The inner payload is the contract event; validate exactly it.
        let contract_event = &value["event"];
        assert_eq!(
            contract_event["type"], kind,
            "the discriminant must be the contract token"
        );
        let violations = contract.validate_thread_event(contract_event);
        assert!(
            violations.is_empty(),
            "`{kind}` does not validate: {violations:?}\nserialized: {contract_event}"
        );
        // The type dispatcher a projection uses must also resolve it.
        assert!(
            contract.thread_event_schema(kind).is_some(),
            "`{kind}` has no schema in the contract's type map"
        );
    }
}

#[test]
fn a_mutation_of_a_field_name_is_caught() {
    // The point of the check above is that a rename fails loudly. Prove the
    // validator would not accept the old-style snake_case shape.
    let contract = Contract::load();
    let wrong = json!({
        "type": "item/agentMessage/delta",
        "threadId": "thr_x",
        "scope": { "kind": "turn", "turnId": "run_x" },
        "providerThreadId": "thr_session",
        "item_id": "assistant-1",
        "delta": "hello",
    });
    assert!(
        !contract.validate_thread_event(&wrong).is_empty(),
        "a snake_case field name must be rejected"
    );

    let renamed = json!({
        "type": "item/agent-message/delta",
        "threadId": "thr_x",
        "scope": { "kind": "turn", "turnId": "run_x" },
        "providerThreadId": "thr_session",
        "itemId": "assistant-1",
        "delta": "hello",
    });
    assert!(
        !contract.validate_thread_event(&renamed).is_empty(),
        "a renamed discriminant must be rejected"
    );
}

#[test]
fn the_terminal_event_is_turn_completed() {
    let contract = Contract::load();
    let (thread_id, project_id, run_id) = ids();
    let event = RunEvent::completed(thread_id, project_id, run_id, 1, Some("p".into()));
    let value = serde_json::to_value(&event).unwrap();
    assert_eq!(value["event"]["type"], "turn/completed");
    assert!(contract.validate_thread_event(&value["event"]).is_empty());
    // The loom-only outcome rides the envelope, not the contract payload.
    assert_eq!(value["outcome"], "completed");
    assert!(value["event"].get("outcome").is_none());
}
