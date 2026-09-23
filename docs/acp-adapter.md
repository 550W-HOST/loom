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
   native)           │  · negotiate v2 first, then v1 fallback │
                     │  · capture identity and capabilities    │
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
| Handshake | SDK protocol connector: v2 first, v1 fallback; capture identity and capability snapshot |
| Session lifecycle | `session/new`, v2 `session/resume` or v1 `session/load`, `session/cancel`, `session/delete` |
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

### The embedded adapter's settle budget

One deadline does belong to the adapter: `pi-acp` linked into the worker fails a
prompt it accepted and never saw settled, so a request cannot hang forever. That
budget bounds **silence**, not the turn — every event the agent sends re-arms
it, and a tool the agent is running holds it off, so a long build, a long
download or a long streamed answer is never mistaken for a stuck agent. A prompt
that never got going (an extension slash command that never enters the agent
loop) is still failed with `settleTimeout`.

loom states the value in `WorkerConfig::settle_timeout` (default 10 minutes,
`--settle-timeout-ms`; `0` disables it) instead of inheriting the adapter's own
default, because the in-process path never reads `PI_ACP_SETTLE_TIMEOUT_SECS`.

### The worker's own run budgets

The worker applies the same rule one layer out, because the adapter's budget only
exists on the embedded path and only bounds a prompt the adapter is *listening*
to. `ProviderRun::timeout` (`--run-timeout-ms`, default 30 minutes) is the run's
silence budget: every reported event re-arms it and an item that started without
completing holds it off, so a long tool — a build, a download, a forked child
agent that runs for half an hour — never trips it. A run that *is* ended this way
says so: the terminal error names the budget that fired
(`the agent produced no events for 1800000ms…`) and its category is
`budget-exceeded`, not `connection-failed`. `ProviderRun::ceiling`
(`--run-ceiling-ms`, default six hours) is the last-resort bound that applies
regardless of activity, and it is the only thing that ends an agent wedged with a
tool call still open. `0` removes either bound.

The control plane repeats the same arithmetic in `RunRecord::refresh_deadline`,
recomputing the deadline from the run's last report and its open items on every
report it applies. It has to: its deadline is the backstop for a *worker* that
went quiet, and a wall clock there would reap runs the worker is still nursing.
The two configurations must be kept consistent: `loom server` takes the same
pair as `--run-timeout-ms` and `--run-ceiling-ms`, and `--local-worker` passes
them to the child it starts, so one set of flags describes both ends of a single
bound. A server ceiling below the worker's would still reap a run the worker is
willing to nurse.

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
| `Plan` / `PlanUpdate` | `TurnPlanUpdated` | v1 and v2 plan shapes |
| `UsageUpdate` | `ThreadTokenUsageUpdated` | field-for-field |
| `SessionInfoUpdate` | `ThreadNameUpdated` when titled | |
| `CurrentModeUpdate` | *none* | v2 has no equivalent; see below |
| `ConfigOptionUpdate` | *none* | informational; logged not published |
| `AvailableCommandsUpdate` | *none* | no timeline fact; reported out-of-band — see below |

### The command list is reported, not logged

`AvailableCommandsUpdate` is the agent's command menu: the slash commands the
session will accept. It is not work, so no contract event can carry it, but it
is also not something the client can derive on its own — a package prompt or a
`skill:` command never appears in the workspace scan. The translator therefore
captures the list instead of dropping it
(`AcpTranslator::take_advertised_commands`) and the session reports it on the
**commands channel**, a side report modelled on the model catalogue
(`ProviderCommandsReport` in `loom-provider-protocol`).

The report is keyed by `(host, provider, cwd)` because prompt files are read
from the session's working directory. `projects.commands` merges it over the
workspace scan: the scan supplies the `origin` and `argumentHint` that ACP's
`AvailableCommand` does not carry, and a name only the advertisement knows is
attributed by its name (`skill:<name>` is `source: skill`, anything else is the
agent's own, `origin: builtin`). A row the scan already answered keeps the
scan's row, so the merge is additive rather than a replacement. Command-menu
updates emitted during `session/load` or `session/resume` are retained too; the
replay guard continues to suppress conversation history.

### Turn lifecycle

ACP has no thread or turn concept at all — it models a *session* and a
*prompt*. loom's contract has `thread/started`, `thread/identity`,
`turn/started`, `turn/input/accepted` and `turn/completed`. **The adapter
synthesizes the missing ones** in `crates/worker/src/acp/session.rs`:

| Event | Source |
| --- | --- |
| `thread/identity` | the ACP `sessionId`, emitted once when it is known |
| `turn/started` | a `session/prompt` was sent |
| `turn/input/accepted` | the prompt was accepted (`PromptResponse` under v1) |
| `turn/completed` | v1's `PromptResponse.stop_reason` or v2's `StateUpdate::Idle` |
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

### Tool result normalization

ACP does not give tool output one universal JSON shape. The adapter reads result text
from the typed `content` text blocks first, then from common `rawOutput` envelopes
(`content[].text`, `text`, `message`, `details.diff`, `stdout`, `output` and
`stderr`). A value with no known text shape is kept as structured JSON for the
generic `ToolCall` row, and is pretty-printed for a specialized item whose
contract only has a text result. This is keyed by the fields present, not by an
agent or tool name: ACP `ToolKind` is a category and extensions may use their
own names.

The v1 adapter applies the same normalization to `ToolCallUpdate`, because the
result commonly arrives only on the terminal patch. v2 applies it to both
`ToolCallUpdate` and `ToolCallContentChunk`. `Search`, `WebSearch` and
`WebFetch` preserve a readable `resultText`; generic calls preserve `result`.

For protocol debugging, `LOOM_ACP_TRACE=1` logs one-line v1 tool start/update
summaries (session, call id, status, content-block count and whether raw output
exists) and keeps the existing v2 typed-update trace for deeper inspection. The
new v1 path reports metadata rather than result bodies, so an operator can
separate "agent sent no result" from "translation or projection dropped it"
without copying OMP tool output into logs.

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
agent ──session/request_permission──▶ adapter
                                        │  InteractionRequest (up the socket)
                                        ▼
                                    server: durable Interaction, published
                                      to thread:{id}; a client answers
                                        │  InteractionResolutionFrame
                                        ▼  (through the relay, to host:{id})
agent ◀──RequestPermissionResponse──── adapter
```

Two frames, in opposite directions, over different transports, and there is
**no default answer**:

| Hop | Carrier | Why |
| --- | --- | --- |
| request | `ClientCommand::InteractionRequest` up the worker's socket | the worker cannot record a durable entity; the control plane can |
| answer | `InteractionResolutionFrame` through the relay to `host:{id}` | the answering client need not be the worker's peer, and a resolution published while the worker was reconnecting must replay |

Mapping is version-native at the ACP edge: v1 and v2 requests are parsed by
separate adapters, then both use the same worker permission broker and the same
control-plane interaction contract.

| ACP | loom |
| --- | --- |
| v1 `RequestPermissionRequest.tool_call` | `InteractionPayload.subject` (`tool_use`) |
| v2 `RequestPermissionSubject::ToolCall` | `InteractionPayload.subject` (`tool_use`) |
| v2 `RequestPermissionSubject::Command` | `InteractionPayload.subject` (`command`) |
| v2 extension/unknown subject | generic `tool_use` projection; raw v2 subject stays in the worker prompt model |
| `options: Vec<PermissionOption>` | `availableDecisions`, derived from option kinds |
| `PermissionOptionKind::AllowOnce` | `allow_once` |
| `PermissionOptionKind::AllowAlways` | `allow_for_session` |
| `PermissionOptionKind::Reject*`, or no options at all | `deny` |
| `Selected { option_id }` | the ACP response, option id chosen by the worker |
| `Cancelled` | interaction cancelled, and the ACP response is `Cancelled` |

### There is no default answer

The previous implementation selected the first allowing option and answered
with it. That is a policy no user consented to, and it **silently approved
operations** — the exact failure the old `system/permissionGrant/lifecycle` note
in `event-model.md` described. It is gone. What replaces it:

* **An unanswered request is cancelled**, after
  `WorkerConfig::permission_timeout` (default 5 minutes, `--permission-timeout-ms`).
  ACP reads `Cancelled` as "not granted"; nothing else in the protocol does.
* **A request the control plane refuses to record is cancelled at once**, rather
  than held open where no client can reach it. The refusal is a run that is not
  in flight, or one this host does not own — the same ownership rules a run
  report gets.
* **A connection that drops settles every open request as cancelled**, so an
  agent is never left blocked on a question whose answer can no longer arrive.

### What a permission decision can and cannot express

The contract's answer to an approval is one of three decisions
(`allow_once` / `allow_for_session` / `deny`), and its response schema rejects an
opaque resolution there. So the decision travels as a **polarity**, and the
worker maps it onto the agent's own options: an `allow` picks the agent's first
allowing option of the matching strength, a `deny` a rejecting one, and a
`deny` with no rejecting option is sent as `Cancelled` because loom must never
answer an allow the user refused.

The lossy case is a permission request whose options are **not distinguishable
by polarity** — ACP's `select` bridge presents one `AllowOnce` option per
choice, so "alpha" and "beta" are both `allow_once`. loom cannot name one:
the contract has no place for an `optionId`, and fabricating a fourth decision
would be inventing vocabulary. The worker therefore takes the agent's own first
matching option, which is the only rule that does not invent a choice the user
did not make. `crates/worker/src/acp/permission.rs` states this at the code, and
`permission::tests::a_multi_choice_request_resolves_by_the_agents_own_ordering`
pins it. A request with the normal ACP shape (one allowing option, one rejecting
one) is unaffected, and so is `confirm` (Yes/No).

`providerThreadId` on the interaction is the ACP **session id**, so a client can
correlate the question with the `providerThreadId` its run events carry.
`turnId` is the run id, because loom's turn *is* the run.

## Capabilities the adapter declares

The client capabilities are intentionally left at the SDK defaults; filesystem
and terminal callbacks are not advertised until loom has handlers for them.

The **agent's** capabilities are read from `initialize` and used to gate
features, never inferred and never worked around:

| Capability | Effect |
| --- | --- |
| v1 `agentCapabilities.loadSession` | a resumed run needs it; without it an explicit resume **fails** rather than silently starting a fresh conversation |
| v1 `agentCapabilities.sessionCapabilities.list` | gates session import; absent means `Unsupported`, and loom does **not** scan the agent's storage in its place |
| v2 `capabilities.session` | gates the baseline `session/new`, `session/resume` and `session/list` surface |
| initialize identity and capabilities | captured as a stable agent identity, negotiated protocol version and opaque capability snapshot for the adapter's caller |

The history-load path is gated by the same table and nothing else: v1 needs
`loadSession`, v2 needs `capabilities.session`. An agent that has neither is
reported as unable to load history rather than being read from its storage —
loom still never parses an agent's session files.

### What a replay does and does not carry

This is the boundary of what a restored conversation can show, and it is a
property of the protocol rather than a bug to be worked around:

| Aspect | What a replay gives |
| --- | --- |
| Roles | user messages, assistant messages and tool results |
| Assistant text | one whole text per message, not the deltas it streamed as |
| Reasoning / thinking | **not replayed** by pi-acp; a restored conversation has no thinking rows |
| Timestamps | none. A replayed row reports no time instead of inventing one |
| Run identity | none. Rows are grouped by a local key that is deliberately not a loom run id |
| Item ids | synthesized per replay, so rows are reconciled by position and content, not by id |

Two consequences are worth stating for whoever reads a restored timeline: it is
the *shape* of the conversation that is restored, not the recording of it, and a
run action cannot be offered from a row that belongs to no run — which is why
the local grouping key must never parse as one.

## Version handling

The Rust adapter uses the SDK protocol connector with v2 first and v1 fallback.
It does not infer protocol behavior from an agent name or a version string:
the selected version is the one returned by `initialize`. v2 completion arrives
as `StateUpdate::Idle` and resume uses `session/resume` without
`replayFrom`; v1 completion arrives in `PromptResponse.stop_reason` and
restore uses `session/load`.

ACP v2 remains an unstable draft in the pinned SDK. The adapter keeps its v2
schema types, message patch handling, terminal lifecycle and capability shape
behind the ACP boundary so the domain and server only see the existing
`ProviderEvent` contract. v1 remains supported for the stable ecosystem: the
default Pi path negotiates v2 (`pi-acp` `v0.5.0` serves it natively), while
other agents in scope still answer v1.

## The session mapping

The session mapping is stored on the thread as the opaque provider session id.
The adapter's capability result also carries the initialize identity, selected
protocol version and opaque capability snapshot; these are adapter metadata and
do not leak ACP schema types into the domain or server. The thread-to-session
relationship remains keyed by the provider binding and workspace.

**The binding is the other half of the mapping.** A provider session id is the
*agent's* identifier, unique only within that agent; a session belongs to the
directory it was opened in; and it is a file on the disk of the machine that
opened it. loom therefore records
`ProviderSessionBinding { agent, cwd, host_id }` alongside the id, from the same
`thread/identity` event, taking the agent, cwd and host from the run that opened
the session rather than from the thread's current environment (which can be
re-bound between runs). The host id is recorded when the worker reports its
identity, and a binding without one resumes nothing.

A dispatch only carries the id when `Thread::may_resume_session(agent, cwd,
host)` holds, and the worker refuses before opening a session when it does not:

| Condition | Outcome |
| --- | --- |
| agent, workspace and host match | v2: `session/resume <id>`; v1: `session/load <id>` |
| agent differs | fresh session — the id means nothing to this agent |
| workspace differs | fresh session — the conversation is about another project |
| host differs | fresh session — the session file is on the machine that opened it, and nothing here can read it |
| no binding (an older snapshot) | fresh session; "cannot prove it is the same conversation" is not a reason to resume |
| id present but the agent has no `loadSession` | **explicit failure**, never a silent fresh start |
| workspace absent on this host | **explicit failure naming the path**, never a silent fresh start |

The last two rows are the difference between "loom did what you asked" and
"loom quietly did something else". A fresh session is recoverable; a resume that
was not what it claimed is not.

A binding recorded before loom started storing the host id has no host to match,
so it resumes nothing: the thread starts a fresh session on its next run. The
storage is still on the machine that made it, but which machine that is was
never written down, and guessing wrong would restore one thread's history under
another's name.

## Loading history

Showing a thread's conversation is a different operation from continuing it, and
`crate::acp::history` is where it lives:

- it opens a connection of its own per load — never the run's connection, so a
  read can never race a turn that is writing to the same session;
- it `initialize`s, checks the negotiated version, then v2 `session/resume
  { replayFrom: start }` or v1 `session/load`;
- it **never sends a prompt**. The connection exists to receive a replay and is
  closed when the replay ends;
- it refuses a permission request rather than answering one: a load is not an
  action, and no tool call in a replayed history is being approved;
- it collects the frames as the same `provider-event` shapes a live run
  produces, so the server projects them with the same folds as live rows;
- it is bounded: a byte budget for the whole replay and a batch size for what
  crosses the wire, both decided by the caller. Past the budget the load fails
  with `too_large` rather than truncating, because a truncated history that
  claims to be the conversation is worse than a refusal.

Two connections on one session happen in practice — a load while a turn streams —
and with pi they are tolerated: measured 2026-09-20, both sides complete and the
session file stays valid. The load returns a **snapshot without the in-flight
turn**, which is the property that matters downstream: a replay is a baseline,
not a live view, so the server refuses to install one whose overlay moved under
it. The probe is `crates/worker/tests/session_race_probe.rs` (`#[ignore]`, needs
credentials). A native agent's own session store may behave differently, and the
probe is the thing to re-run before assuming it does not.

The replay's completion is a protocol property, not a timeout: the SDK's
dispatch loop delivers notifications before the `session/load`/`session/resume`
response, so the response is the end of the replay. Nothing here waits for a
quiet period to decide the history is finished — that would make a slow agent's
history look short. The one non-protocol property is that a *failed* connection
still discards what it collected; no partial replay is installed as a baseline.

## Session import is capability-gated

`session/list` is how an agent's existing sessions are enumerated, and it is
optional. `crate::acp::sessions` runs the probe and returns one of three
outcomes: `Listed`, `Unsupported` (the agent does not advertise the capability),
or `Failed` (a missing executable, a refused handshake, a deadline).

**`Unsupported` is not an empty list.** "This agent cannot list" and "this agent
has no sessions" are different facts, and collapsing them would make an
unsupported capability look like an empty account. No branch of the probe walks
an agent's session directory: that knowledge belongs to the adapter that
already has it. See `docs/provider-sessions-research.md`.

The probe's client cancels any permission request it receives: a listing has no
thread to record an interaction against and no client that could answer one, and
cancellation is the only reply that is never an approval.

## Where the code goes

| Piece | Location | Why |
| --- | --- | --- |
| Adapter, transport, translation | `crates/worker/src/acp/` | It is the sole provider execution path |
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

1. **Golden mapping tests** — feed recorded ACP v1 and v2 frames, assert the
   `ProviderEvent` sequence.
2. **Item lifecycle** — a tool call start→update→complete produces one
   `ItemStarted` and one `ItemCompleted` with the merged shape.
3. **Data-driven variant selection** — the same `ToolKind` maps to a specific
   variant when its required data is present and to `ToolCall` when it is not.
   This is the rule most likely to rot.
4. **Terminal invariant** — every path ends in exactly one `TurnCompleted`,
   including cancelled and failed.
5. **Protocol negotiation** — fake v1 and v2 agents complete initialize,
   new/resume, prompt, streaming and completion without duplicate terminal
   events.
6. **Embedded pi-acp** — the whole thing over `Channel::duplex()` against a
   mock Pi, no child adapter process.

## Open questions

- **`presentation`**: the contract's `ItemPresentation` is rich (label, icon,
  badge, tint). ACP has no equivalent. Omit it (null) and let the UI default, or
  synthesize from `ToolKind`? Omitting is honest; synthesizing duplicates UI
  concerns into the adapter.
- **`fs` capabilities in step 1**: declaring them makes agents route file I/O
  through loom (better for remote machines); not declaring them has each agent
  touch the filesystem directly (fine on the same host, wrong for remote). This
  interacts with the worker's user model (W-558).
- **Where permission *policy* lives**: the adapter must answer, but "auto-allow
  read-only tools" is a user preference, not an adapter constant.
