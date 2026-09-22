# The bb contract and loom

loom reuses bb's UI and execution plane, so `loom-server` has to speak the HTTP
and WebSocket contract bb already defines. Reading bb's TypeScript to learn that
contract does not scale and cannot be enforced, so this document describes the
machine-readable export under `contracts/bb/` and the decision it encodes.

## Decision: loom's public surface is bb's contract, not a variant

There are two protocol surfaces in loom, and they have different rules.

**1. UI-facing surface — adopt bb's contract wholesale (subset, no additions).**
The UI is the product app built from bb's client source and compiled into
`loom-server`, and bb's client code is the consumer. A UI cannot negotiate a
dialect, so anything the UI can see must be byte-compatible with bb. That
covers:

- `/api/v1/*` HTTP routes, shapes and error bodies,
- `/ws` client messages (`subscribe`/`unsubscribe`/`ping` -> `changed`/`pong`),
- `/ws/terminals/:terminalId` terminal messages.

loom-specific frames must never be sent to a bb client. The contract is a lower
bound, not a suggestion.

**2. Server↔worker surface — intentional divergence.** bb's daemon protocol is
replaced because the worker is a Rust rewrite (`loom-worker`) and the relay
owns delivery. The relay envelope in `crates/server/src/protocol.rs`
(`subscribe { scope }`, `event { event_id, scope, payload }`, `enroll_host`,
`run_report`, ...) is the **internal transport**, not the client protocol. It is
still captured here as `host-daemon.json` so the Node execution plane can be
checked in against it later, but loom is allowed to differ. This is the
"明确分歧" the issue asked for: the divergence is real and is confined to the
worker half of the wire.

### Current wire split

`crates/server/src/protocol.rs` now exposes two disjoint message unions:

- `/ws` accepts only bb `subscribe`/`unsubscribe`/`ping` messages when the
  client explicitly negotiates `loom-bb-realtime-v1`; it emits only
  `changed`/`pong`.
- `/internal/ws` carries worker enrollment, scoped relay delivery, reports and
  replay under `PROTOCOL_VERSION`.

No `Origin` or user-agent heuristic selects a protocol. During the v2 to v3
migration only, a `/ws` connection that offers no subprotocol receives a legacy
`welcome` carrying v3 and is immediately closed. That single refusal frame lets
an already-deployed v2 worker enter self-update; it cannot enroll or send an
internal command on the public endpoint. New workers connect directly to
`/internal/ws` and receive the versioned `hello` frame.

The loom-native HTTP control endpoints remain contract-external and are listed
explicitly in `docs/api-coverage.md`; they are not counted as bb routes.

## Artifact format: JSON Schema, not OpenAPI

The export is **JSON Schema 2020-12 plus a manifest**, one file per surface.

Why not OpenAPI:

- Two of the four surfaces are not HTTP. bb's UI `/ws`, terminal `/ws` and the
  worker `/internal/ws` are WebSocket message protocols. OpenAPI cannot express them, so
  an OpenAPI document would cover at most a third of the contract and hide the
  rest.
- The contract is generated from zod and TypeScript, not spec-first. zod v4
  emits JSON Schema 2020-12 natively; wrapping that in OpenAPI adds a lossy
  layer with no consumer.
- The conformance target is a Rust server that must match *shapes*. JSON Schema
  is directly consumable by a small validator (`crates/contract/src/schema.rs`)
  with no spec parser.

The one thing OpenAPI would have given us — a route table — is a first-class
field of `server-api.json` instead.

## The artifacts

Regenerate with `scripts/export-bb-contract.sh <bb-checkout>`. Never edit them
by hand. `manifest.json` records the bb revision and the hash of every file.

| File | Kind | Contents |
| --- | --- | --- |
| `server-api.json` | `bb-http` | 167 routes with `request { source, schema }` and `responses [{ status, format, schema }]`, plus `errorResponse` (`apiErrorSchema`) and `lifecycleErrors` |
| `client-ws.json` | `bb-client-ws` | `client` and `terminal` protocols, subscription targets, change kinds |
| `host-daemon.json` | `bb-host-daemon` | daemon commands, results by type, WebSocket messages, enrollment/session/event/tool/interaction shapes, protocol version |
| `error-codes.json` | `bb-error-codes` | error code -> status inventory scanned from `apps/server/src` throw sites |
| `thread-event.json` | `bb-thread-event` | the complete `ThreadEvent` union and a schema for every `type` discriminator |

Shared definitions live in each file's `$defs`; every `$ref` is a local
`#/$defs/<name>` pointer, so a file is self-contained.

## Conformance testing

`crates/contract` embeds the artifacts and exposes lookups and validators:

```rust
let contract = loom_contract::Contract::load();
let route = contract.http_route("GET", "/api/v1/system/version").unwrap();
let violations = contract.validate_response(route, 200, &body);
assert!(violations.is_empty(), "{violations:?}");
```

`Violation` carries a JSON path, so a failure names the field that is wrong.
The same API covers client messages (`validate_client_message`), server
messages (`validate_server_message`) and worker frames
(`validate_worker_message`, `validate_server_to_worker_message`).
Thread projection code can query `thread_event_schema("item/started")` or
validate a complete event with `validate_thread_event`.

`crates/contract/tests/conformance.rs` guards the artifacts themselves: refs
resolve, every JSON route has a response schema, and the validator accepts
contract-shaped values and rejects malformed ones.

## Request conformance is enforced at runtime

A response can be asserted in a test because the test can see it. A request
cannot: a handler that silently reshapes its body still returns the right JSON,
so response assertions stay green while a client sending the contract's shape
is rejected. That is exactly what happened to B1's write routes (W-554), which
accepted `{ "project_id": ... }` while `threads.create` requires
`{ "projectId", "origin", "input", "environment" }`.

Two mechanisms close it:

1. `loom-server` wires `validate_contract_request` as middleware. For every
   contract route whose `request.source` is `json`, the parsed body is
   validated before the handler runs, and a mismatch is a `422` in the uniform
   `{ code, message }` error shape naming the offending field. Contract-external
   loom routes are untouched. Live matching needs `Contract::match_route`, which
   treats the contract's `:id` parameters as wildcards.
2. Every implemented JSON-body route has a `validate_request_by_id` assertion in
   `crates/contract/tests/conformance.rs`, and
   `scripts/check-api-coverage.mjs` fails if such a route lacks one. That is the
   regression guard: the coverage number cannot claim a request conformance the
   tests do not prove.

`Contract::shared()` returns the process-wide parse so the middleware does not
re-parse the artifacts per request.

### A consumer of a shape is the third side

Conformance proves the server speaks the contract; it says nothing about the
programs that read the server. `scripts/verify-release-binaries.sh` is one of
those programs, and by construction its failures cannot show up above: it runs
only in the `v*` tag pipeline, so a shape change that breaks it stays invisible
until a release is cut — which is how W-554 shipped, every test green and the
release failing on `POST /api/v1/projects`.

The script's requests, and the jq expressions it parses responses with, are
therefore repeated in `crates/server/tests/release_verification.rs` against a
real listener, on the `cargo test --workspace` path. That is cheaper than
building a musl pair in CI to run the script itself, and it covers the endpoints
the script consumes that are shape-sensitive — `/health`, `hosts.list`,
`projects.create`, `projects.list`; the UI-serving routes it also fetches
already have in-crate coverage in `crates/server/src/ui.rs`. When the script
starts parsing something new, add it there: that test is the script's shape
contract, not the API's.

### Adding a route and keeping both sides consistent

1. Run `scripts/export-bb-contract.sh <bb-checkout>` in the same change that
   pulls a new bb revision. The artifacts and manifest update together. The
   checkout's HEAD must be the revision the manifest records; the manifest
   stamps `git rev-parse HEAD`, so exporting from a different revision silently
   changes the pin.
2. Implement the handler in `loom-server`, taking the contract's request shape
   directly (camelCase, required fields required). Do not accept a loom-only
   dialect beside it.
3. Add a conformance test that captures the handler's actual response and
   asserts it against the contract route, and a `validate_request_by_id`
   assertion for the request shape it accepts and one it must refuse (see the
   module comment in `crates/contract/tests/conformance.rs`).
4. If the route cannot conform, that is a **contract change, not a test
   waiver** — decide deliberately, document it here, and if it affects the UI
   surface reconsider the divergence.

The export fails loudly when a bb export is renamed (`schemaFrom` throws) and
reports type-only responses under `failures.opaqueResponseTypes`, so the Rust
side can never silently conform to a contract that has drifted.

## B2: routes that differ from bb on purpose

Batch B2 (thread control and auxiliary views) implemented fourteen routes whose
success shapes match the contract exactly. Five behaviours inside them are
deliberate divergences, recorded here because "the route cannot conform" is a
decision, not a test waiver:

- **`threads.compact` answers `501 not_configured`.** Compaction asks the
  provider to summarise its own context, and `loom_provider_protocol` has no
  such frame: a dispatch carries a prompt, nothing more. The worker only ever
  *observes* compaction — Pi decides, and the bridge maps `compaction_end` to
  `thread/compacted` (`docs/event-model.md` row 7) — so there is no direction in
  which one can be requested. Answering `{ "ok": true }` for a compaction that
  never happened is the failure the batch's acceptance criteria name, so the
  refusal is explicit and carries a code from the contract's own list at the
  status that code declares. The route becomes implementable when the protocol
  grows a request frame; the report path that would carry the result exists.
- **`threads.editMessage` answers `501 not_configured`.** Editing a sent
  message rewrites a turn the provider already executed. loom's conversation is
  the relay log, which is append-only, and the provider protocol has no rewind
  frame (loom's own provider capabilities report `supportsSessionRewind:
  false`). Appending the edited text instead would leave both messages in the
  conversation — a different conversation, not an edit.
- **A scheduled retry (`sendAt` in the future) was deferred to B3.** B2
  answered `501 not_configured` because a deferred turn needs the queued-message
  surface; **B3 landed that surface, so the route now answers the contract's
  `delivery: "queued"` branch** (see the B3 section below). This paragraph is
  kept as the record of why B2 could not implement it.
- **`threads.retry` no longer refuses a busy thread.** B2 answered `409
  conflict` for a run in flight; B3 makes it the queued branch, because the
  contract declares one and B2's refusal was explicitly a placeholder for it.
- **`threads.stop` cancels on the control plane only.** The stop shares the run
  lifecycle (`finish_run` with `RunOutcome::Cancelled`), so the thread scope
  receives one terminal event and the thread returns to `idle`. The worker is
  not told, because the provider protocol has no cancel frame; the provider
  process runs to its own end and its later reports are dropped as unknown runs,
  exactly as a superseded run's already are. The route is idempotent: a thread
  with no run in flight is the state the caller asked for.
- **`threads.open` publishes into the room and reports local fan-out.** The
  open request travels the only path the control plane has — the thread's relay
  room — and `delivered` is the hub's subscriber count for that room. Loom has
  no ephemeral frame path by design, so an open request is retained and
  re-delivered on replay; the frame is idempotent for a client that receives it
  twice.

Two more projections are documented where they are implemented rather than
here, because they are shape-complete and only partially sourced:
`threads.search` walks the threads it holds (no index) and
`threads.update`'s `model`/`reasoningLevel` are recorded and reported but not
yet carried into a dispatch, because `ProviderSpec` has no field for them.

## B3: interactions and the queue

Batch B3 (interactions, plan control and queued sending) added the two entity
kinds the contract requires and fourteen routes over them. The interesting
decisions are semantic, and each is recorded here because the route alone does
not show it.

### Two new domains, both durable

A **queued message** and an **interaction** are entity-view rows, not handler
state. Both are in `RegistrySnapshot`, both publish a `DomainEvent` to the
thread scope (`thread_queued_message_changed`, `thread_interaction_changed`)
and both replays in `DomainRegistry::apply_event`, so a restart does not lose
anything a client can see. The snapshot's two new fields are `#[serde(default)]`,
which is what lets a snapshot written by an older build load: it simply has no
queue and no pending interaction, exactly as that build would have described.

Their state machines are small and total, and both are exercised as legal and
illegal transitions (`crates/domain/src/queue.rs`, `crates/domain/src/
interaction.rs`, plus the live-registry test in
`crates/server/tests/b3_conformance.rs`):

```text
queued message:  queued ──send──▶ sent        (terminal)
                    │
                    └──cancel──▶ cancelled   (terminal)

interaction:     pending ──resolve/respond──▶ resolved     (terminal)
                    │
                    └──cancel────────────────▶ interrupted  (terminal)
```

`resolving` is modelled but not entered today: a resolution settles in one step
because the provider protocol cannot confirm it (see below). The status exists
anyway, because a client renders it and inventing its meaning later would be the
breaking change.

### The three interaction verbs are three different operations

* **`respond`** carries an opaque `value` and is stored as a `request_answer`.
  It only answers a `generic` (or `plugin`) interaction — one whose body loom
  does not interpret.
* **`resolve`** carries a **typed** resolution (a permission decision, a set of
  question answers, a plugin submission, a `request_answer`) and is validated
  against the interaction's own kind. A decision cannot answer a question; a
  question's answers cannot answer an approval. A mismatch is `400
  invalid_request`, not a stored row nothing will read.
* **`cancel`** settles the interaction as `interrupted` with **no answer**. It
  is not a denial: a denial is a decision the provider receives, whereas a
  cancellation is loom giving up on a question the provider will never get an
  answer to (because the run was stopped, or the worker went away).

A settled interaction refuses a second answer with `409
awaiting_user_interaction`, which is a status the contract declares for that
code. `finish_run_with` settles every interaction a thread still had open when
its turn ended, so `hasPendingInteraction` in the thread list cannot be stuck.

**Where interactions come from.** ACP's `session/request_permission` is the
producer, wired in W-566. The worker holds the request open and sends
`ClientCommand::InteractionRequest` up its socket; `AppState::record_interaction_request`
records a durable interaction and publishes `thread_interaction_changed` to the
thread scope; a client answers over the interaction routes; and
`AppState::deliver_interaction_resolution` publishes
`InteractionResolutionFrame` through the relay to `host:{id}`, where the worker's
broker hands it to the agent's blocked request.

That is a frame in each direction, which is what the earlier batch deliberately
waited for. Three properties are worth stating here because they are the ones a
client or a reviewer will check:

* **A request is only recorded for a run in flight and owned by the requesting
  host**, the same ownership rule `apply_run_report` enforces. A question for a
  run nobody is advancing is refused, because no client could render it in a
  timeline.
* **The interaction id is derived from `(run_id, request_id)`.** ACP's request
  ids are unique only within a session, so scoping the hash by run is what keeps
  a restarted worker's fresh question from colliding with a settled row from an
  earlier run. The provider's own id stays verbatim in `origin`, so the answer
  frame can name it.
* **No client, no answer.** The worker cancels an unanswered request after its
  permission timeout, cancels it immediately when the control plane refuses to
  record it, and cancels every open request when the connection drops. A
  cancellation is never an approval. See `docs/acp-adapter.md`.

`providerThreadId` on the interaction is the agent's own session id when it has
one, falling back to loom's thread id only when it does not.

### `threads.clearGoal` publishes an event; `threads.clearContext` refuses

loom has no goal entity. A goal is a projection of the thread's own run log: the
contract's `thread/goal/updated` and `thread/goal/cleared` events are the only
record, exactly as the `@bb/thread-view` projection the app renders timelines
with treats them (`extractThreadTimelineGoal` in
`ui/packages/thread-view/src/goal-snapshot-extraction.ts`). `threads.clearGoal`
therefore publishes `thread/goal/cleared` and is idempotent — the next
`threads.timeline` read reports `goal: null`. Nothing was
added to the entity view, because a stored goal would be a second source of
truth that could disagree with the log.

`threads.clearContext` answers `501 not_configured`. Clearing a context means
emptying the **provider's** ACP session memory. The session is owned by the ACP
agent and can only be changed through a provider-supported lifecycle method;
dropping loom's records while the agent carried on would be the same silently
wrong answer. The current provider protocol has no context-clear command.

### `threads.cancelPlan` refuses

A plan is the provider's own working state, reported through
`turn/plan/updated`. Cancelling means telling the provider to stop pursuing it,
and `loom_provider_protocol` has dispatch, provision and report and nothing
else. Publishing a "cancelled" plan would change what a client displays while
the provider kept executing it — a silently-wrong answer — so the route answers
`501 not_configured` and names `threads.stop` as the operation that does work.

### `threads.eventWait` is a bounded poll with a null timeout

The cursor is the same one `threads.events` uses: the row's `seq`, derived from
the thread room's replay order, and `afterSeq` is **exclusive**. The relay has
no "wait for a new frame" primitive and deliberately cannot have one — a
producer never learns who is subscribed — so the wait is a bounded poll of the
log every 25 ms.

`waitMs` defaults to **30 s** and is capped at **60 s**: a long poll with no
ceiling is a connection leak with a friendly name. A timeout answers **`200`
with a JSON `null`**, which is the contract's declared second branch, not an
error: a client tells "nothing yet" from "here is what happened" without an
error path. A match answers the bare `ThreadEventRow` `threads.events` would
return.

### `threads.timelineTurnSummaryDetails` reuses the timeline's projection

It is a **filtered view of `threads.timeline`**, not a second projection: rows
come from the same cached row set the timeline itself serves
(`cached_thread_timeline_rows`), so a row returned here is byte-identical to the
same row in the timeline it came from. What differs is the
selection (a turn's `[sourceSeqStart, sourceSeqEnd]` range) and the paging
direction — `beforeCursor` walks **backwards**, because a UI expands a collapsed
turn from its newest summary row towards its oldest. The filter is on source
sequence rather than on the event's turn scope, because the user message that
opened the turn is thread-scoped by construction and is exactly the row a
summary expansion anchors on. `historySnapshot` is always `null`: loom keeps no
snapshot of a turn's pre-compaction history, and a fabricated one would be worse
than the null the contract allows.

### Queued sending has two honest outcomes

`threads.sendQueuedMessage`'s `mode` is honoured, not ignored:

| mode | thread idle | thread busy |
| --- | --- | --- |
| `auto` | sent | queued (`waitingOn: thread-busy`) |
| `steer` | sent | steered: the row is marked sent and its text joins the running turn |

`steer` while busy publishes a `RunSteer` to the host that owns the run and
marks the queued row sent; the contract's `sent` branch carries no
`queuedMessage`, so the row's `thread_queued_message_changed` event is what
removes it from a client's queue. When the run ended between the status check
and the publish there is no turn to join, so the message falls back to an
ordinary delivery and becomes the next turn — exactly the `appliedAs:
"new-turn"` fallback bb's host daemon makes.

A send of a message whose `sendAt` is still in the future answers the queued
branch too — but a **manual** send is authoritative over the schedule (the
client is asking for it now), while the **automatic** drain respects it. That is
the difference between `deliver_queued_message(force = true)` and
`drain_thread_queue`.

### `threads.send`'s modes and `threads.retry`'s queued branch

B3 completes what B2 deferred. `threads.send` now honours `mode`:

| mode | thread idle | thread busy |
| --- | --- | --- |
| `start` | sent | `501 not_configured` |
| `auto` / `queue-if-active` | sent | queued |
| `steer` / `steer-if-active` | sent | steered: sent, joined to the running turn |

A **steer** joins the turn in flight rather than starting a second one. The
control plane appends the message to the timeline and publishes a `RunSteer`
through the relay to the host that owns the run. ACP has no "inject into the
running prompt" method, so the worker delivers the text the way every ACP client
does: it cancels the prompt in flight and re-prompts on the **same session**,
keeping the run open so its one terminal event still comes from the provider.
That is Zed's "send immediately" and bb's `steerMode: "queue"` bridge, and the
run reads as one turn rather than two. A steer that loses the race on the
control plane — no run in flight by the time it is handled — is sent as a fresh
turn (bb's `appliedAs: "new-turn"`); one that loses it on the worker is already
recorded on the thread and is seen by the next turn. Neither case drops the
message.

A future `sendAt` is always a queue entry, never an immediate turn. A
`threads.retry` of a busy thread, or one with a future `sendAt`, is likewise a
queued message with a `retry` payload (`retryOfTurnRequestId`, `attempt`,
`reason`) — the contract's second response branch — instead of B2's `409`. The
retry is delivered by the same drain as any queued message, which is what makes
"retry after the current turn" mean what a client would expect.

`attempt` counts the runs the thread's log already shows. It is derived from
every entry into `working` — a retry issued from `error` and one issued after a
`stop` differ in their trigger but not in what the client asked for — plus the
queued-retry rows that have not started yet, so a second queued retry reports
`2` rather than `1`.

### The queue drains on a terminal run, and on the reconciler

`finish_run_with` calls `drain_thread_queue` after the thread's status change,
and `reconcile_runs` calls `drain_due_queued_messages`. The first is what makes
"queue while busy" work at all; the second is the backstop for a `sendAt` that
arrives while nothing else happens, and for a message left queued by a crash
between a run's terminal event and the drain.

The drain stops at the first message it cannot send. The queue is ordered, and
skipping a blocked message to deliver a later one would reorder the conversation
against the client's arrangement — so exactly one message follows one finished
run, which is what a queue means.

## B5: thread files and storage helpers

Batch B5 (thread counts, pane actions and thread file access) added ten routes.
Seven of them read a filesystem, and the interesting decision is **whose**
filesystem that is.

### A file read is a request to the host, not a local read

A thread's workspace and its thread storage live on the host its environment
names. The control plane must never open its own disk and present the result as
that thread's file: on a multi-machine deployment the two are different
machines, and on a single-machine one the answer would be right by accident and
wrong by design. So `loom-provider-protocol` gained a third request/report pair:

```text
  server ── HostFileRequest ──▶ relay host:{id} ──▶ worker
  server ◀── HostFileReport ── worker socket (host_file_report)
```

The request travels **through the relay** for the same reason a dispatch does: a
worker that was reconnecting receives it on replay, and replaying a read is
harmless because reading is idempotent. The answer comes back **up the worker's
own socket**, not through the room, because it satisfies exactly one waiting
HTTP request — fanning a file's contents out to every client watching the host
would be a leak with no reader.

Because the relay is one-way by design (a producer never learns who is
subscribed), the waiting HTTP request is parked in `crate::host_files::HostFileBroker`
under a fresh correlation token and woken by the socket task when the report
arrives. A report whose token is unknown is dropped rather than treated as an
error: a timeout, a redelivery and a client that hung up all produce one.

`AppState::request_host_file` is the **only** way any route reads a host file,
and it refuses a host that is not enrolled before publishing — a request into a
room nobody is in could only time out. The timeout is `30 s`, matching bb's
`COMMAND_TIMEOUT_MS`.

### Thread storage is named from the host's own data directory

`<data_dir>/thread-storage/<thread_id>` is bb's layout, and it is the **worker**
that owns it. The control plane cannot know a machine's data directory unless
that machine says so, so `enroll_host` gained an additive `data_dir` field, and
`Host` records it. `threads.storageLocation` is then answered from the entity
view without asking the host at all: it is a question about the layout, and a
client opening a storage panel should not fail because the worker is briefly
away.

A host that never reported a data directory — an older worker, or a host
enrolled through the reference HTTP endpoint — answers **`501 not_configured`**
on the storage routes. Inventing a path on a machine the server does not own is
the failure this refuses to commit. The value survives a reconnect and is only
replaced by a *new* report: an enrollment that omits it cannot erase it.

### Three permission scopes, and the traversal defence has two halves

The routes are not interchangeable, and their roots differ on purpose:

| scope | routes | root |
| --- | --- | --- |
| workspace | `worktreeFile` | the environment's own workspace path |
| storage | `storageContent`, `storageFile`, `storageFiles`, `storagePaths`, `storageLocation` | the host's data directory plus the storage layout |
| absolute host | `hostFileContent`, `rawFile` | none — the client names an absolute path |

Relative paths are validated **before** a request is built: NUL, a leading `/`,
a backslash, and any `.`/`..`/empty segment are refused with `400 invalid_path`,
mirroring bb's `parseSafeRelativeRoutePath`. That check cannot see symlinks and
cannot see through a path assembled on another machine, so a read that names a
root is re-checked on the host, which resolves both sides and refuses anything
that escapes. Both halves are tested: the server test proves a `..` never
reaches the host, and the worker test proves a symlink out of the root is
refused.

The absolute-host scope is deliberately **not** root-confined — the client is
pointing at a file it already knows the location of (an image in a timeline, a
tool's log) — but it is still confined to the thread's own host. A relative
path is refused there, because it cannot name a file the client meant.

### A thread with no environment is refused, not defaulted

Every file route resolves the thread's environment first. A thread bound to no
environment answers `409 thread_environment_unavailable` and a workspace read on
an environment with no path answers `409 environment_not_ready`, both of which
the contract declares. Falling back to the thread's project, to the primary host
or to the server's own directory would each answer a different question than the
one asked.

### `threads.count` is a count over the entity view

It is computed from the same rows `threads.list` would return, so the sidebar
count and the sidebar list cannot disagree. `includeArchived` and
`includeHidden` are independent, deleted threads never count, and `providerId`
amounts to the one configured provider rather than a per-thread field nothing
sets. `parentThreadId` is three-valued as the contract specifies: omitted does
not filter, `none` is roots only, and any other value is that parent's id.
`groupBy=host` keys a thread with no environment under `null`, which is what the
contract's nullable key is for.

### `threads.paneAction` publishes, like `threads.open`

The request travels the only path the control plane has — into the thread's
relay room — and `delivered` counts that room's local subscribers. It is a
fan-out count, not an acknowledgement, and it is idempotent for a client that
receives it twice. The frame is `thread_pane_action_requested`, carrying the
thread, its project and the action.

### `If-None-Match` is ignored on file content

bb's daemon returns an entity tag derived from the file's SHA-256 and answers a
matching `If-None-Match` with `304`. Loom's `HostFileContent` carries no content
hash, and a tag synthesised from the size and path would be wrong in the
direction that matters: a same-length edit in place would keep the old tag and a
client would render stale bytes. So every content read is a `200` with the
bytes. The route stays shape-compatible — the contract declares a binary `200`
and nothing else — and this is recorded rather than silently approximated.
Adding a hash to the host protocol is the change that would make `304`
implementable.

## B7: project workspace, attachments and thread sections

Batch B7 added fourteen routes: the project workspace's files, paths and
content; its prompt commands, prompt history, attachment upload/read/copy,
source update, reorder and delete; and the three thread-section routes.

### Project files are a host question, and the resolver is three-way

`projects.files`, `projects.paths` and `projects.fileContent` are B5's rule
applied to a project: the bytes live on the machine that holds the project's
code, so the control plane asks that machine through `HostFileOperation` and
never reads its own disk. No new protocol was needed for the reads.

The contract's query parameters make the resolution three-way, and the order is
the decision:

1. `environmentId` names an environment; its host and path win, because a
   client that opened a specific worktree means *that* workspace.
2. Otherwise `hostId` picks the project's source on that machine, so a
   multi-host project can be read on the machine the client is looking at.
3. Otherwise the project's **default** source is used — the one a workspace is
   provisioned from — and a project with no source is a `404 not_found`.

A source with an empty path (a repository declared but not yet checked out) is
a `409 conflict`, not an empty directory: those are different facts. As in B5,
relative paths are refused with `400 invalid_path` before a request is built,
and the worker re-resolves the real path against the workspace root.

### Attachments are written by the host, inside a root it enforces

`projects.uploadAttachment` is a **multipart** body, so the runtime request
validator is a no-op for it and the handler owns the whole validation surface.
Axum's `multipart` feature was already enabled for the routing layer; the
handler rejects a missing file part, an empty file, a file over 16 MB, and a
name that reduces to nothing. Only the final path segment of a client-supplied
name survives, so `../../etc/passwd` cannot name a destination.

Storing one is a `HostFileOperation::Write` the worker confines to
`<data_dir>/project-attachments/<project_id>` — the sibling of B5's thread
storage, named from the same reported data directory, with the same `501
not_configured` when a host never reported one. The bytes travel base64-encoded
so binary uploads survive the JSON hop. The worker resolves the target's
**parent** against the root before joining the file name, because the target
does not exist yet and a symlinked parent is exactly the escape a prefix
comparison would miss.

`projects.copyAttachments` copies one project's attachments into another's, so
`HostFileOperation::Copy` carries **two** roots: sources are confined to the
source project's directory and the destination to the target's. A copy across
two different hosts is refused with `409 conflict` rather than silently moving
bytes through the control plane. A source that is missing or oversized is
reported per path, and a colliding destination name is suffixed (`a-2.png`)
rather than overwriting: a copy must never lose a file the user still had.

### `projects.commands` is a host RPC with a projected result

A prompt-command list is a property of the workspace on disk — project prompts
live under `<cwd>/.pi/prompts`, user prompts under the agent's own directory —
so this is a `HostRpcOperation::ListCommands` against the project's source. The
discovery itself is `pi-acp`'s (`load_slash_commands` plus the built-in command
list), so loom does not maintain a second, drifting notion of where a slash
command lives. The worker answers raw rows and the control plane projects them
into bb's `projectCommandSchema`: a file command's `origin` is read from the
`(user)`/`(project)` label `pi-acp` puts on it, a built-in is `origin: builtin`
with its declared description and argument hint, and `source` is `command`
unless the name is a skill. The contract's `provider` parameter is required,
and a value naming a provider this server does not run is a `400` rather than a
silently different answer.

A live session's advertisement is merged over the scan. Every ACP session
advertises its command list right after `session/new` or `session/load`;
`AvailableCommandsUpdate` carries no timeline fact, so the worker reports it on
its own frame (`ProviderCommandsReport`) keyed by `(host, provider, cwd)`, and
the server keeps the latest list per workspace in memory. The merge is
**additive**: the scan's row wins for a name it already answered (it is the one
with an `origin` and an `argumentHint`), and a name only the advertisement knows
is attributed by what its name says — `skill:<name>` becomes
`source: skill, origin: user`, anything else the agent advertises beyond the
scan becomes the agent's own (`origin: builtin`). A workspace no session has
run in yet answers from the scan alone, and an advertisement is only as fresh
as the last session in that workspace.

### `projects.reorder` needed a real project order

`Project` gained an optional `sort_key`, and the list orders by archived status,
then the key, then creation time and id. A project that has never been reordered
has no key and still sorts by creation time, so an existing workspace looks
exactly as it did.

The first reorder — or any reorder whose bounding neighbours are unranked —
rewrites the whole visible list into a contiguous key range. That is deliberate:
synthesising a key for an unranked neighbour would place the new rank *before*
every unranked project rather than between the two rows the client named. After
one reorder every project holds a key, and later inserts take the cheap
single-row path. A stale neighbour or a pair already in the requested order is a
`409`, not a silent no-op.

### `projects.delete` is a tombstone, and refuses rather than cascades

Deletion sets `deleted_at_ms`, exactly like a thread's tombstone, so replaying
an older `project_created` cannot resurrect it. A deleted project is absent from
`projects.list` and the sidebar bootstrap, and resolving it by id is a `404`. It
is refused while the project still holds a live thread or a live environment
(`409 conflict`): those threads would otherwise name a project the client can no
longer open. Archiving being weaker than deleting, an archived project *can* be
deleted — refusing that would strand it.

### Thread sections are durable now

`ThreadSection` is an entity (`sec_…`), persisted in the domain snapshot and
listed in `sidebarBootstrap.sections`, which previously hardcoded `[]`. Names
are unique after trimming, which is what makes the contract's `409` meaningful;
the routes use the contract's own `section_not_found` and
`section_name_conflict` codes.

Deleting a section **counts** the threads that referenced it and does not
rewrite them. The thread's stored grouping is the client's last write, and
re-filing every thread would be an unrequested mutation of a different entity.
A client that wants its threads back in the default group sends `threads.update`
itself, which is the only path that already owns that field.

### Error codes: no `project_source_not_found`

A project with no source on the named host, or with no source at all, answers
`404 not_found`. The contract lists no `project_source_not_found`, and inventing
a code a client cannot branch on is worse than the generic one.

## B9: workspace file operations and terminal sessions

Batch B9 added seventeen routes: eight file operations (`files.list`,
`listPaths`, `mkdir`, `move`, `read`, `remove`, `write`, plus B8's
`createPreview`) and nine terminal routes (`create`, `get`, `list`, `input`,
`output`, `resize`, `close`, `restart`, `update`). Both families are, without
exception, questions for the machine that owns the file or the process.

### Files: the host is the only party that touches a path

Every file route follows B5/B7's rule. The control plane resolves the target
host — an explicit `hostId`, or the primary connected host — and publishes a
`HostFileRequest`; the worker performs the operation and reports the outcome.
The three scopes are:

- **Root-confined** (`mkdir`, `write`, `move`, `remove`, `read`) — `rootPath` is
  **required**, and the path is treated as root-relative. It is validated for
  `..`/NUL/backslash/absolute escapes here and then re-checked on the worker
  against the *canonicalised* path, so a symlink inside the root cannot leave
  it. A route in this family with no root is a `400 invalid_request` before
  anything is published: the route's job is to require the boundary, even
  though the worker would tolerate its absence.
- **Absolute-path** (`list`, `listPaths`, `read` with no root) — the client
  names a path it already knows, still executed on that host.
- **Preview capability** — B8's short-lived root-bound lease; B9's file routes
  do not widen it.

`files.write` carries the contract's `expectedSha256` as a **tri-state**, and
the distinction is load-bearing: absent means "no check", `null` means "the file
must not exist yet" (create-only), and a string means "the file must hash to
this". A plain `Option<String>` would collapse the first two and turn a
create-only write into an unconditional overwrite, so the presence of the JSON
key is what is captured. A mismatch answers `200` with
`{outcome: "conflict", currentSha256}`, exactly as the contract declares — a
conflict is a successful comparison, not a transport failure.

The worker writes through a sibling temp file and renames it into place, so a
crash mid-write leaves either the old file or the new one, never a truncated
file that looks like a successful save. The write path reports the SHA-256 of
the bytes it just wrote as a new optional field on `HostFileContent`, so
`files.write` can answer the contract's `sha256` without a second round trip.

A path-targeting `HostFileOperation` family was added rather than reusing B7's
upload semantics: `CreateDirectory`, `Move`, `Remove`, `ReadWithMetadata`,
`WriteFile`, `SetMetadata`, and `CopyPath`. B7's `Write` is deliberately *not*
reused by `files.write`, because B7's write exists to avoid clobbering a
colliding name (it suffixes), while an editor's save must replace the file and
honour `expectedSha256`. `move` defaults to no-overwrite for the same reason an
upload does: silently losing a file the user still had is the failure this
design keeps refusing.

### Terminals: a process on a machine, driven by request

A terminal is a PTY with a child process, and it lives on exactly one host. The
control plane keeps an **index** (identity, ownership, size, status) and never
a PTY handle, never a byte of output. `TerminalRequest`/`TerminalReport` mirror
the file protocol — the same correlation token, the same host-ownership check,
the same bounded answer — with one difference that matters: a terminal is
**stateful on the host**, so the report carries a per-session output sequence.

The target decides the host, and the ownership:

| target | host | owning entity |
| --- | --- | --- |
| `thread` | the thread's environment's host | the thread and its environment |
| `environment` | the environment's host | the environment |
| `host_path` | the named `hostId` | none (a host-scoped shell) |

A thread with no environment, or an environment with no workspace path, is a
`409` (`thread_environment_unavailable` / `environment_not_ready`) before
anything is published. A `host_path` target with no `cwd` uses the host's own
reported data directory, because the control plane cannot invent a path on a
machine it does not own.

#### Output is bounded on the worker, and the cursor is explicit

A terminal's stdout is unbounded. Buffering it in the control plane would turn
"a command printed a lot" into "the server ran out of memory", so each session
owns a **bounded ring** on the worker: at most 4096 chunks and 8 MiB of decoded
bytes, oldest dropped first. `terminals.output` reads a window
(`sinceSeq`/`limitChunks`/`tailBytes`, all clamped) and always answers with
`nextSeq` — the cursor to pass next time — and `truncated`, which says whether
anything was dropped *before* this window. A reader that falls behind therefore
loses old chunks and is told so, which is a fact it can render; an unbounded
buffer would instead silently hold every byte forever. `nextSeq` is the
session's head when the window is empty, so a reader that polls an idle terminal
does not have to distinguish "nothing new" from "no such session".

#### Lifecycle, reconnect and cleanup

```text
  create ─▶ starting ─▶ running ─(child exits)─▶ exited
                │           │
                │           ├─ close(force) ──▶ exited (user)
                │           └─ restart ───────▶ starting
                └─ spawn failed ─────────────▶ exited (process-exit)

  server connection drops ─▶ session marked `disconnected` (process survives)
  worker reconnects ───────▶ the server asks for a full inventory and reconciles
```

A terminal belongs to its **process and its user**, not to the server
connection. So a dropped connection marks the record `disconnected` rather than
killing the process — the worker keeps it running — and a reconnect reconciles:
the worker is the authority on liveness, and `TerminalOperation::Report` returns
every session it holds. A session the host no longer knows (a worker restart, a
machine reboot) is closed as `daemon-disconnect`; one it still holds is
returned to `running`. A **clean worker shutdown**, by contrast, kills every
session it holds: they are its own child processes, and leaving them running
with no worker to report them would leak processes the control plane could never
see again.

A thread being deleted or archived, and an environment being destroyed, settle
their terminals' records immediately and ask the host to kill the processes in
the background. The lifecycle change must not fail because a host is
unreachable: an orphaned process on a disconnected machine is a leak, but
refusing to delete the thread would be a much worse one.

`terminals.update` is the exception to the request rule: a title is
control-plane metadata that changes nothing about the process, so a rename never
touches a host and a disconnected session can still be renamed.

#### One deliberate divergence in `files.read`

The contract's `filesReadResponseSchema` declares `mimeType` as nullable but
required, and `content-encoding` as `base64` | `utf8`. The worker *does* compute
a best-effort media type for the content routes (B5/B7 use it for a
`content-type` header), but `files.read` deliberately reports `null` rather than
inventing a type: the client that calls this route is an editor that already
knows what it opened, and a wrong specific type is worse than an honest absence.
This is the same reasoning B5 records for its `content-type`, applied where the
field is data rather than a header.

#### Why `terminals.close` can answer `terminal_not_running`

The contract's `mode` is `force` or `if-clean`, and the distinction is real:
`if-clean` closes a session that has already exited and is refused (`409
terminal_not_running`) while the process is still alive, because silently
killing a running shell the user asked to close "if clean" would be the wrong
reading. The contract only accepts `reason: "user"` on this route, so a
different reason is a `400` rather than a close with a mislabelled cause.

## Known limits

- **Error codes are best-effort.** bb's contract package types the error body
  (`{ code, message, details?, retryable? }`) but leaves `code` a free string;
  `error-codes.json` is scanned from throw sites under `apps/server`.
- **`pattern` is carried but not enforced** by the Rust validator: it has no
  regex dependency, and a wrong dialect is worse than no check.
- **Recursive types lose precision.** A zod schema that recurses cannot be
  inlined; the recursion point becomes an unconstrained schema. This is rare
  (a handful of plugin/json-value shapes) and noted in the manifest.
- **The export is large-ish (~1.2 MB) and generated.** Repeated substructures
  are interned into `$defs` to keep it reviewable; a diff that touches a domain
  shape will still touch several places.
- **The `properties` key is never interned to a `$ref`.** The intern pass must
  not treat a property map as a schema; before W-554 it did, turning 87 shapes
  (the `threads.create`/`projects.create` request bodies among them) into
  `"properties": { "$ref": ... }`. The Rust validator reads that as an empty
  property set with a stray key, so every valid instance was reported as a
  violation. `tools/contract-export/src/intern.ts` handles `properties`
  specially and the conformance test `every_declared_schema_resolves` would not
  have caught it.
