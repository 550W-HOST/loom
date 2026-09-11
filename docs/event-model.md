# The event model, aligned with bb's `ThreadEvent` contract

loom reuses bb's projection layer (`ui/packages/thread-view`) to turn an event
stream into a timeline. That layer **dispatches on `event.type` and reads
camelCase fields**, so the wire representation is the contract: renaming a
discriminant or a field silently breaks the timeline. This document records how
loom's event model maps onto the contract exported by the prerequisite issue —
`contracts/bb/thread-event.json`, generated from bb's
`packages/domain/src/provider-event.ts`.

## What changed

`RunEvent` used to be a seven-variant enum of loom's own design
(`Started / Output / ToolCall / ToolResult / Turn / Notice / Finished`). The
projection layer cannot consume that: it separates `item/started` from
`item/completed`, reasoning text from the assistant answer, a plan delta from a
file change, and so on.

The model is now:

- **`loom_domain::ProviderEvent`** — the contract's 35 provider event types, one
  variant each, internally tagged by `type` with the exact contract tokens.
- **`loom_domain::ThreadEventItem`** — the contract's item union (18 kinds),
  used by `item/started`, `item/completed`, the background-task and delegation
  progress events.
- **`loom_domain::ThreadEvent`** — `{ threadId, scope, …body }`, exactly the
  flattened object bb stores and serves.
- **`loom_domain::RunEvent`** — loom's envelope around a `ThreadEvent`
  (`thread_id`, `project_id`, `run_id`, `at_ms`, and an optional loom-only
  `outcome`). The envelope is loom's; the inner `event` is contract-shaped.
- **`loom_domain::ThreadEventType`** — all 48 contract discriminator tokens:
  35 provider types plus the 13 `client/*` / `system/*` types. `parse` accepts
  any token; `provider()` rejects a client/system token explicitly instead of
  inventing a fallback body.

`crates/domain/src/event.rs::DomainEvent::ThreadRunEvent` carries a `RunEvent`
(flattened into the domain event), so a client dispatching on `thread_run_event`
finds a contract event at `event.event`.

### Scope

A run maps onto exactly one turn: the contract's `scope.turnId` is the run id.
That is what makes a turn survive a server restart — the id is not reassigned.
Thread-scoped facts (identity, name, goal, background tasks, delegations, rate
limits, resolved environment, extension state) use `{"kind":"thread"}`.

## Per-type decisions

Coverage legend: **produced** = loom emits it today; **other form** = the fact
exists but the contract type is not emitted verbatim, with the reason;
**not produced** = no producer in loom, stated explicitly rather than left
blank. "server-only" means only the control plane could author it, not a
provider bridge.

### Provider event types (35)

| # | contract type | coverage | producer / reason |
| --- | --- | --- | --- |
| 1 | `thread/started` | not produced | Thread creation is `DomainEvent::ThreadCreated` before a run exists; no provider frame starts a thread. A turn opens with `turn/started`. |
| 2 | `thread/identity` | produced | daemon; the run's first event. `providerThreadId` is the thread id, which the daemon also passes as `--session-id`. |
| 3 | `turn/started` | produced | daemon; Pi `agent_start`. |
| 4 | `turn/completed` | produced | daemon (Pi `agent_settled`, a rejected prompt, exit, timeout) and server (deadline, stale host, restart, no host). The single terminal event. |
| 5 | `turn/input/accepted` | not produced | loom dispatches one prompt synchronously; acceptance is the `turn/started` boundary. There is no client request id to echo yet. |
| 6 | `thread/name/updated` | not produced | loom sets the provider session name through argv, not a frame; Pi never reports a rename back. |
| 7 | `thread/compacted` | produced | daemon; Pi `compaction_end` when it succeeded. |
| 8 | `thread/context/cleared` | not produced | Pi's RPC mode has no context-clear notification; `new_session` is an operation loom does not drive. |
| 9 | `thread/goal/updated` | not produced | Pi exposes no goal object through its RPC events. |
| 10 | `thread/goal/cleared` | not produced | as above. |
| 11 | `item/started` | produced | daemon; Pi `tool_execution_start`, `compaction_start`. |
| 12 | `item/completed` | produced | daemon; Pi `tool_execution_end`, `text_end`, `thinking_end`, `compaction_end`, and the assistant flush. |
| 13 | `item/agentMessage/delta` | produced | daemon; Pi `message_update` / `text_delta`. |
| 14 | `item/commandExecution/outputDelta` | produced | daemon; Pi `tool_execution_update` for a command tool, with `reset: true` because Pi sends an accumulated snapshot. |
| 15 | `item/fileChange/outputDelta` | not produced | loom closes a file change in one `item/completed`; Pi streams no separate file-change output. |
| 16 | `item/reasoning/summaryTextDelta` | not produced | Pi's thinking frames carry reasoning text, not a distinct summary channel; that text maps to `item/reasoning/textDelta`. |
| 17 | `item/reasoning/textDelta` | produced | daemon; Pi `message_update` / `thinking_delta`. |
| 18 | `item/plan/delta` | not produced | Pi's RPC event list has no plan/todo delta; a plan would arrive as ordinary assistant text or a tool item. |
| 19 | `item/mcpToolCall/progress` | not produced | Pi's tool updates do not distinguish MCP servers; they map to `item/toolCall/progress`. |
| 20 | `item/toolCall/progress` | produced | daemon; Pi `tool_execution_update` for a non-command tool. |
| 21 | `item/backgroundTask/progress` | not produced | Pi has no background-task concept. |
| 22 | `item/backgroundTask/completed` | not produced | as above. |
| 23 | `item/delegation/progress` | not produced | Pi has no delegation concept; a delegated thread would be a loom thread, not a provider item. |
| 24 | `item/delegation/completed` | not produced | as above. |
| 25 | `thread/tokenUsage/updated` | produced | daemon; the assistant `usage` block in Pi `agent_end`. |
| 26 | `thread/contextWindowUsage/updated` | not produced | Pi reports context usage only through the `get_session_stats` command, which loom does not issue. |
| 27 | `turn/plan/updated` | not produced | no Pi plan frame (see #18). |
| 28 | `turn/diff/updated` | not produced | no Pi working-tree diff frame; a diff would be derived from `fileChange` items. |
| 29 | `provider/error` | produced | daemon; Pi `agent_end` with `stopReason: error`, `auto_retry_*` failure, compaction failure, a rejected prompt. |
| 30 | `provider/rateLimits/updated` | not produced | Pi's RPC events do not carry rate-limit state. |
| 31 | `provider.env-resolved` | not produced | loom resolves env at spawn time in the daemon process; it is not a Pi event. |
| 32 | `thread/extensionState/updated` | not produced | loom has no plugin/extension system, by decision. |
| 33 | `provider/warning` | produced | daemon; Pi `extension_error`, a declined interactive dialog, a skipped compaction. |
| 34 | `provider/modelFallback` | not produced | Pi does not report a model fallback as an event. |
| 35 | `provider/unhandled` | produced | daemon; any frame with a real payload that has no contract type. This is the contract's own diagnostic type, **not** a loom catch-all: it carries the raw frame, and it is emitted only when the frame is genuinely unclassified. |

### Client and system types (13)

These are authored by a server or a client, never by a provider, so
`ProviderEvent` has no variant for them. They are still enumerated in
`ThreadEventType` so an incoming token is classified exactly.

| contract type | coverage | producer / reason |
| --- | --- | --- |
| `client/thread/start` | server-only | A future client protocol emits this; today thread creation is `DomainEvent::ThreadCreated` on the project scope. |
| `client/turn/requested` | server-only | The dispatch exists (`RunDispatch`), but loom does not yet record a thread-scoped request event with a client request id. |
| `client/turn/rejected` | server-only | A rejected dispatch is `RunEvent::failed`; the dedicated rejection event arrives with the client request path. |
| `client/turn/start` | server-only | as `client/turn/start` above. |
| `system/error` | server-only | loom errors are HTTP/`ServerMessage::Error` frames; no thread-scoped system error event yet. |
| `system/manager/user_message` | server-only | loom user messages are `DomainEvent::ThreadMessageAdded`, not provider events. |
| `system/thread/interrupted` | server-only | A cancellation is currently `turn/completed` with `status: interrupted`; the thread-level interruption event is future UI work. |
| `system/operation` | not produced | loom has no thread-management operation model. |
| `system/interaction/lifecycle` | not produced | loom has no interactive-client model. |
| `system/permissionGrant/lifecycle` | not produced | loom's bridge auto-declines a provider dialog (no UI is attached); the decline is surfaced as `provider/warning`. A permission UI would emit this type instead. |
| `system/userQuestion/lifecycle` | not produced | as above: a question dialog is auto-declined and surfaced as `provider/warning`. |
| `system/thread-provisioning` | not produced | Environment provisioning emits `environment_status_changed` on the project scope, not a thread event. |
| `system/provider-turn-watchdog` | not produced | bb's legacy persisted diagnostic; nothing produces it. |

## The terminal-event invariant

Every run ends in exactly one `turn/completed`. The contract's `status` is
`completed | failed | interrupted`, which collapses several failure modes, so
loom also records its own verdict in the envelope's `outcome`
(`completed | failed | timed_out | host_stale | cancelled`). That field is
**not** part of the contract event: the inner payload validates against
`contracts/bb/thread-event.json` unchanged. The thread lifecycle transition is
chosen from `outcome`; a projection ignores it.

## Required projection fields

The fields the projection needs are all present on the contract events:

| what | where |
| --- | --- |
| item id | `itemId` on deltas/progress, `item.id` on `item/*` |
| turn id | `scope.turnId` (the run id) |
| tool call id | `item.id` for a tool item |
| parent tool call id | `parentToolCallId` on items and deltas |
| timestamps | `RunEvent::at_ms` (envelope) and item `durationMs` |
| thinking vs text channel | distinct types: `item/reasoning/*` vs `item/agentMessage/*` |

## The stdout guard is unchanged

`guard_stdout_line` still runs before the frame mapper: only a JSON object with
a string `type` becomes a frame, and everything else goes to stderr. The new
model widens what a *frame* can become; it does not widen what counts as a
frame. bb #1180 (Pi's OSC 777 notification wedging a turn) stays prevented.

## Enforcement

- `crates/contract/tests/event_model.rs` asserts, against the exported
  schema, that **every** contract type is classified, that all 35 provider
  types have a sample, and that each sample's serialized `RunEvent` validates.
  A renamed field or discriminant fails there.
- `crates/daemon/tests/provider_e2e.rs::a_provider_turn_runs_end_to_end_and_is_replayable`
  runs a provider whose stdout is deliberately polluted and validates every
  produced frame.
- `the_real_pi_process_streams_through_the_bridge` (ignored by default, runs
  the real `pi` CLI) validates a real turn's frames the same way.
