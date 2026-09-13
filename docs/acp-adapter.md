# The ACP adapter boundary

Step 1 of the provider migration: what the ACP adapter is responsible for, what
it consumes and produces, and where the line sits between it and the rest of
loom. This is a contract between components, not an implementation.

Read `provider-strategy.md` first for *why* ACP; this document is *what* and
*where*.

## The boundary in one picture

```
                     ┌─────────────────────────────────────────┐
  ACP agent  ──ACP──▶│            ACP adapter                  │
  (pi-acp or         │                                         │
   native)           │  · initialize ACP v1 and capture caps   │
                     │  · own the session lifecycle            │
                     │  · translate updates → ProviderEvent     │
                     └──────────────┬──────────────────────────┘
                                    │ ProviderEvent + RunEvent
                                    ▼
                            existing run path
                     (RunDispatch in, ProviderReport out)
```

The adapter is a **translator with state**, not a new layer. It produces the
same event type consumed by the existing run path, so everything above it — the
run record, relay, projection and UI — is unchanged.

## What the adapter owns

| Responsibility | Detail |
| --- | --- |
| Transport | Spawn a native ACP agent (`Stdio`), or drive an embedded `pi-acp` (`Channel::duplex()`) |
| Handshake | `initialize` with ACP v1; capability capture |
| Session lifecycle | `session/new`, `session/resume`/`session/load`, `session/cancel`, `session/delete` |
| Update translation | `SessionUpdate` → `ProviderEvent`, with the state that requires |
| Client callbacks | Answer `session/request_permission`; declare `fs`/`terminal` capabilities it actually implements |
| Unmapped updates | Log with the discriminator; never coerce |

## What the adapter does not own

- **Timeline invariants.** Item ids are the adapter's, but turn/item lifecycle,
  ordering and the one-terminal-event rule stay in the run path (`RunOutcome`).
- **The session mapping's persistence.** The adapter *reports*
  `(agent, session_id, cwd)`; the domain snapshot stores it.
- **Whether a run is alive.** Deadlines and reconciliation are the control
  plane's (`crates/server/src/runs.rs`).
- **Permission policy.** The adapter asks; *who decides* is a client concern.

## The event mapping

`ProviderEvent` is bb's 35-type contract (`crates/domain/src/provider_event.rs`).
ACP offers fewer, coarser updates, so the mapping is many-to-one in places and
must not invent detail.

### Direct mappings

| ACP `SessionUpdate` | `ProviderEvent` | Notes |
| --- | --- | --- |
| `UserMessageChunk` | `ItemAgentMessageDelta` (role user) | see "roles" below |
| `AgentMessageChunk` | `ItemAgentMessageDelta` | the common case |
| `AgentThoughtChunk` | `ItemReasoningTextDelta` | thought ≈ reasoning |
| `ToolCall` | `ItemStarted` | first sight of a `toolCallId` |
| `ToolCallUpdate` | `ItemToolCallProgress` + `ItemCompleted` | status decides which |
| `Plan` | `TurnPlanUpdated` | v1's only plan update |
| `UsageUpdate` | `ThreadTokenUsageUpdated` | field-for-field |
| `SessionInfoUpdate` | `ThreadNameUpdated` when titled | |
| `CurrentModeUpdate` | *none* | v2 has no equivalent; see below |
| `ConfigOptionUpdate` | *none* | informational; logged not published |
| `AvailableCommandsUpdate` | *none* | UI affordance, no timeline fact |

### Turn lifecycle

ACP has no thread or turn concept at all — it models a *session* and a
*prompt*. loom's contract has `thread/started`, `thread/identity`,
`turn/started`, `turn/input/accepted` and `turn/completed`. **The adapter
synthesizes the missing ones** in `crates/daemon/src/acp/session.rs`:

| Event | Source |
| --- | --- |
| `thread/identity` | the ACP `sessionId`, emitted once when it is known |
| `turn/started` | a `session/prompt` was sent |
| `turn/input/accepted` | the prompt was accepted (`PromptResponse` under v1) |
| `turn/completed` | v1's `PromptResponse.stop_reason` (v2 will use `StateUpdate::Idle`) |
| `thread/started` | **not emitted** — the contract scopes it to thread creation, which the domain already recorded |

ACP v1 and v2 signal completion differently, and this is the one place the
adapter's shape is version-dependent:

- **v1**: `session/prompt` *responds* with `PromptResponse { stop_reason }`.
- **v2**: `PromptResponse` is an empty ack; completion arrives as
  `StateUpdate::Idle { stop_reason }`, and start as `StateUpdate::Running`.

Both must produce exactly one `TurnCompleted`. The `StopReason` mapping:

| ACP `StopReason` | `RunOutcome` |
| --- | --- |
| `EndTurn` | `Completed` |
| `MaxTokens` / `MaxTurnRequests` | `Completed` (the turn ended; it is not an error) |
| `Refusal` | `Failed` |
| `Cancelled` | `Cancelled` |

`MaxTokens` mapping to `Completed` rather than `Failed` is deliberate: the model
stopped, the run succeeded. A caller that wants to continue sends another turn.

### Item identity

`ThreadEventItem` needs a stable per-item id; ACP supplies one only for tool
calls (`toolCallId`). For everything else the adapter mints ids, following the
existing convention:

| Item kind | Id source | Range |
| --- | --- | --- |
| Tool call | `ToolCall.tool_call_id` | as given |
| Assistant message | minted | `assistant-<n>` |
| User message | minted | `user-<n>` |
| Reasoning block | minted | `reasoning-<n>` |
| Plan | minted | `plan-<n>` |

The adapter mints `assistant-<n>` for message items; ACP's `toolCallId` remains
the stable id for tool items.

**Roles**: the contract's `ItemAgentMessageDelta` is the assistant's delta
channel. ACP's `UserMessageChunk` carries the user's own message echoed back.
Mapping it into the same event would render the user's text as the agent's
answer, so the adapter emits a user message item (`ItemStarted` with
`ThreadEventItem::UserMessage`) and does not route its text through the
assistant delta. This matters when an agent replays a prompt.

### Tool calls: `ToolCall` then `ToolCallUpdate`

ACP sends a full `ToolCall` once and then `ToolCallUpdate` patches, where every
field is optional — only changed fields appear. The adapter therefore keeps a
per-`toolCallId` record and merges, because `ProviderEvent`'s `ItemCompleted`
carries a whole `ThreadEventItem`.

`ToolKind` selects the item variant, since the contract distinguishes them —
**but only when ACP actually supplies the data the variant requires.**

This is the sharpest constraint in the whole mapping. ACP tool calls are
*generic*: a `title`, a `kind`, an optional free-form `raw_input`, and a
`content` array. loom's contract is *specific*: `CommandExecution` has a
**required** `command` and `cwd`, `FileChange` a required `changes[]` with paths,
`Search` a required `mode` and `query`. An adapter that always mapped by kind
would have to invent those strings.

So the rule is: **map to the specific variant when the required data is present,
otherwise to `ToolCall`.** Never fabricate a command or path to satisfy a shape.

| `ToolKind` | maps to | when |
| --- | --- | --- |
| `Read` | `FileRead` | `locations[0].path` or `raw_input.path` exists |
| `Edit` / `Delete` / `Move` | `FileChange` | `content` carries `ToolCallContent::Diff` |
| `Search` | `Search` | `raw_input.query` exists; `mode` from the args or `Content` |
| `Execute` | `CommandExecution` | `raw_input.command` exists |
| `Fetch` | `WebFetch` | `raw_input.url` exists |
| `Think` | `Reasoning` | always (its fields are optional) |
| `SwitchMode` | *no item* | a mode change is not work |
| any of the above, data missing | **`ToolCall`** | always available: needs only `id`, `tool`, `status` |
| `Other` | `ToolCall` | |

The generic `ToolCall` variant requires only `id`, `tool` and `status`, all of
which ACP always supplies, so it is the accurate representation of a tool whose
arguments loom cannot interpret — not a fallback in the forbidden sense. `tool`
takes the `ToolKind` and `title` carries the human-readable description.

**File changes are better under ACP than under the Pi path.** `ToolCallContent::Diff`
gives `path`, `old_text` and `new_text` per file, so `FileChangeKind` is derived
from fact (`old_text` absent → `Add`, present → `Update`) rather than inferred.
The adapter maps ACP tool data directly from `raw_input` and `content`; it does
not carry a second provider-specific argument parser.

A terminal-backed tool call (`ToolCallContent::Terminal`) maps to
`CommandExecution` with the terminal id in `presentation`, since the contract has
no terminal item. If it also carries a `command` in `raw_input`, so much the
better; if not, it is a `ToolCall`.

### Status mapping

| ACP `ToolCallStatus` | `ItemStatus` |
| --- | --- |
| `Pending` | `Pending` |
| `InProgress` | `Pending` |
| `Completed` | `Completed` |
| `Failed` | `Failed` |

## Permissions become interactions

`session/request_permission` is a *request from the agent*, not a notification,
and it is the producer the interaction routes were missing.

```
agent → adapter: session/request_permission { tool_call, options[] }
adapter → domain: Interaction { kind: Approval, payload: { options } }
                 (blocks the turn until answered)
client → adapter: outcome (option_id | cancelled)
adapter → agent:  RequestPermissionResponse
```

Mapping:

| ACP | loom |
| --- | --- |
| `RequestPermissionRequest.tool_call` | `InteractionPayload` subject |
| `options: Vec<PermissionOption>` | `availableDecisions` |
| `Selected { option_id }` | `Resolution` for that option |
| `Cancelled` | interaction cancelled |
| `PermissionOptionKind::Allow*` / `Reject*` | decision polarity |

Note this **replaces** the old direct-provider auto-cancel path. ACP permission
requests are handled by the adapter callback and the current policy selects an
allow option or cancels when none exists.

## Capabilities the adapter declares

The client capabilities are intentionally left at the SDK defaults for now;
filesystem and terminal callbacks are not advertised until loom has handlers for
them. The permission callback is handled locally with the current non-blocking
policy.

## Version handling

The current Rust adapter speaks ACP v1, which is the stable version used by the
pinned `pi-acp` dependency. v1 completion arrives in `PromptResponse.stop_reason`;
`session/load` is used for restore. ACP v2 negotiation and its `session/resume`
shape are deliberately not enabled yet.

## The session mapping

The session mapping is stored on the thread as the opaque provider session id;
the current implementation records v1 as the protocol. A future version-aware
adapter can add the negotiated protocol without changing the thread-to-session
relationship.

**The `cwd` must exist on resume.** If it does not, fail with an explicit error
naming the path. Never silently start a fresh session.

## Where the code goes

| Piece | Location | Why |
| --- | --- | --- |
| Adapter, transport, translation | `crates/daemon/src/acp/` | It is the sole provider execution path |
| Session mapping type | `crates/domain/` | It is persisted state |
| Event types | unchanged | The adapter produces existing `ProviderEvent`s |
| Dispatch | `crates/provider-protocol/` | `ProviderSpec` gains an ACP variant |

`ProviderSpec` names an ACP launch. `AcpStdio` starts a native ACP agent as a
child; `AcpEmbeddedPi` connects the same client to `pi-acp` in-process. There is
no direct Pi JSON-RPC launch arm.

## What deliberately stays out

- **No `provider/acp` catch-all event.** An unmapped ACP update is logged, not
  published as a synthetic event. Same rule as W-538's refusal of
  `provider/unhandled`.
- **No fabricated tool arguments.** A `ToolKind` whose required data is absent
  becomes a generic `ToolCall`, never a `CommandExecution` with an empty
  `command` or a `Search` with an invented query. This is why the mapping is
  data-driven rather than kind-driven.
- **No plan synthesis.** `pi-acp` emits no plan updates; the adapter does not
  invent one from tool calls.
- **No file watching for tool locations.** `ToolCallLocation` is advisory; using
  it to update the UI's open file is a client decision, not part of the mapping.
- **No `fs/*` implementation in step 1.** Declared only when implemented.

## Test obligations

The adapter is testable without a real agent, which is why this boundary is
worth defining before writing it:

1. **Golden mapping tests** — feed recorded ACP v1 frames, assert the
   `ProviderEvent` sequence.
2. **Item lifecycle** — a tool call start→update→complete produces one
   `ItemStarted` and one `ItemCompleted` with the merged shape.
3. **Data-driven variant selection** — the same `ToolKind` maps to a specific
   variant when its required data is present and to `ToolCall` when it is not.
   This is the rule most likely to rot.
4. **Terminal invariant** — every path ends in exactly one `TurnCompleted`,
   including cancelled and failed.
5. **Embedded pi-acp** — the whole thing over `Channel::duplex()` against a mock
   Pi, no child adapter process.

## Open questions

- **`presentation`**: the contract's `ItemPresentation` is rich (label, icon,
  badge, tint). ACP has no equivalent. Omit it (null) and let the UI default, or
  synthesize from `ToolKind`? Omitting is honest; synthesizing duplicates UI
  concerns into the adapter.
- **`fs` capabilities in step 1**: declaring them makes agents route file I/O
  through loom (better for remote machines); not declaring them has each agent
  touch the filesystem directly (fine on the same host, wrong for remote). This
  interacts with the daemon's user model (W-558).
- **Where permission *policy* lives**: the adapter must answer, but "auto-allow
  read-only tools" is a user preference, not an adapter constant.
