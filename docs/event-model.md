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

- **`loom_domain::ProviderEvent`** — the contract's provider event types, one
  variant each (34 of the 35; see `provider/unhandled` below), internally
  tagged by `type` with the exact contract tokens.
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
| 2 | `thread/identity` | produced | worker; the run's first event after `session/new` or `session/load`. `providerThreadId` is the ACP agent's opaque session id. |
| 3 | `turn/started` | produced | worker; synthesized before `session/prompt` because ACP has no turn event. |
| 4 | `turn/completed` | produced | worker (ACP prompt stop reason, connection failure, timeout) and server (deadline, stale host, restart, no host). The single terminal event. |
| 5 | `turn/input/accepted` | not produced | loom dispatches one prompt synchronously; acceptance is the `turn/started` boundary. There is no client request id to echo yet. |
| 6 | `thread/name/updated` | not produced | ACP session metadata is translated only when the agent sends a concrete title; the current adapter does not receive a Pi-specific rename command. |
| 7 | `thread/compacted` | produced | worker; an ACP adapter's compaction update when one is available. |
| 8 | `thread/context/cleared` | not produced | ACP has no provider-neutral context-clear update that the current adapter drives. |
| 9 | `thread/goal/updated` | not produced | The current ACP adapter does not synthesize a goal object from agent text or tool calls. |
| 10 | `thread/goal/cleared` | not produced | as above. |
| 11 | `item/started` | produced | worker; ACP `tool_call` and related item-bearing updates. |
| 12 | `item/completed` | produced | worker; terminal ACP tool updates, message flushes and compaction updates. |
| 13 | `item/agentMessage/delta` | produced | worker; ACP `agent_message_chunk`. |
| 14 | `item/commandExecution/outputDelta` | not produced | ACP tool output is currently represented by generic tool progress; no command-output accumulator is synthesized. |
| 15 | `item/fileChange/outputDelta` | not produced | The current ACP mapping closes a file change as one item; it does not invent output deltas. |
| 16 | `item/reasoning/summaryTextDelta` | not produced | ACP thought chunks map to reasoning text, not a separate summary channel. |
| 17 | `item/reasoning/textDelta` | produced | worker; ACP `agent_thought_chunk`. |
| 18 | `item/plan/delta` | not produced | ACP plan updates map to `turn/plan/updated`; no item-level plan delta is synthesized. |
| 19 | `item/mcpToolCall/progress` | not produced | ACP tool calls are not distinguished as MCP calls by the current adapter. |
| 20 | `item/toolCall/progress` | produced | worker; non-terminal ACP `tool_call_update`. |
| 21 | `item/backgroundTask/progress` | not produced | ACP has no mapping in the current adapter. |
| 22 | `item/backgroundTask/completed` | not produced | as above. |
| 23 | `item/delegation/progress` | not produced | Delegation is a loom thread operation, not an ACP item in the current adapter. |
| 24 | `item/delegation/completed` | not produced | as above. |
| 25 | `thread/tokenUsage/updated` | not produced | The current ACP v1 mapping receives context occupancy, not a token breakdown. |
| 26 | `thread/contextWindowUsage/updated` | produced | worker; ACP `usage_update`, including the usage snapshot retained during `session/load`. |
| 27 | `turn/plan/updated` | produced | worker; ACP `plan` update. |
| 28 | `turn/diff/updated` | not produced | The current ACP adapter does not derive a working-tree diff. |
| 29 | `provider/error` | produced | worker; rejected prompt or ACP transport failure. |
| 30 | `provider/rateLimits/updated` | not produced | The current ACP mapping does not expose rate-limit state. |
| 31 | `provider.env-resolved` | not produced | loom resolves env at spawn time in the worker process; it is not a Pi event. |
| 32 | `thread/extensionState/updated` | not produced | loom has no plugin/extension system, by decision. |
| 33 | `provider/warning` | produced | worker; declined ACP permission requests and adapter warnings. |
| 34 | `provider/modelFallback` | not produced | No ACP model fallback event is mapped today. |
| 35 | `provider/unhandled` | **not produced** | An unmapped ACP update is logged and produces no fabricated contract event. |

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
| `system/interaction/lifecycle` | not produced | loom has no interactive-client model. ACP permission requests become `thread_interaction_changed`, a domain event, not a provider event. |
| `system/permissionGrant/lifecycle` | not produced | ACP's `session/request_permission` is bridged to a durable interaction (`docs/acp-adapter.md`), so the decision reaches a user instead of being auto-declined. The contract's dedicated lifecycle type remains unproduced because loom's interaction entity is the richer record. |
| `system/userQuestion/lifecycle` | not produced | ACP v1 has no question channel loom exposes; a bridged permission request is an approval, not a question. |
| `system/thread-provisioning` | not produced | Environment provisioning emits `environment_status_changed` on the project scope, not a thread event. |
| `system/provider-turn-watchdog` | not produced | bb's legacy persisted diagnostic; nothing produces it. |

## The `provider/unhandled` non-decision

bb declares `provider/unhandled` as the diagnostic a bridge emits for a frame
it could not map. loom **classifies the token** — `ThreadEventType` and
`ProviderEventType` both list it, so the union is total and an incoming frame
is never "unknown" when it is really this — but `ProviderEvent` has no
`ProviderUnhandled` body variant and the bridge never constructs one.

The reason is the issue's own constraint: a fallback type is a place for
mapping gaps to hide. An unmapped ACP update is logged with its raw
payload and produces no event, so a new agent variant shows up as a visible
gap rather than a timeline row nobody reads. Adding a variant later is a
deliberate decision with a test, not a default.

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

## ACP framing

ACP owns JSON-RPC framing and the worker SDK consumes complete requests,
responses and notifications. The worker does not parse Pi's private JSONL
protocol; only embedded `pi-acp` talks to Pi internally.

## Enforcement

- `crates/contract/tests/event_model.rs` asserts, against the exported
  schema, that **every** contract type is classified, that every provider
  type except the deliberately unmodelled `provider/unhandled` has a sample,
  and that each sample's serialized `RunEvent` validates. A renamed field or
  discriminant fails there.
- `crates/worker/tests/provider_e2e.rs::a_provider_turn_runs_end_to_end_and_is_replayable`
  runs an ACP agent stub and validates every translated contract event.
- `the_real_pi_process_streams_through_the_bridge` (ignored by default, runs
  Pi through embedded `pi-acp`) validates a real turn's events the same way.
