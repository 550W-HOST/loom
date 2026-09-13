# The bb contract and loom

loom reuses bb's UI and execution plane, so `loom-server` has to speak the HTTP
and WebSocket contract bb already defines. Reading bb's TypeScript to learn that
contract does not scale and cannot be enforced, so this document describes the
machine-readable export under `contracts/bb/` and the decision it encodes.

## Decision: loom's public surface is bb's contract, not a variant

There are two protocol surfaces in loom, and they have different rules.

**1. UI-facing surface — adopt bb's contract wholesale (subset, no additions).**
The plan is to serve bb's UI bundle unchanged (`LOOM_UI_DIR`), and bb's client
code is the consumer. A UI cannot negotiate a dialect, so anything the UI can
see must be byte-compatible with bb. That covers:

- `/api/v1/*` HTTP routes, shapes and error bodies,
- `/ws` client messages (`subscribe`/`unsubscribe`/`ping` -> `changed`/`pong`),
- `/ws/terminals/:terminalId` terminal messages.

loom-specific frames must never be sent to a bb client. The contract is a lower
bound, not a suggestion.

**2. Server↔daemon surface — intentional divergence.** bb's daemon protocol is
replaced because the daemon is a Rust rewrite (`loom-daemon`) and the relay
owns delivery. The relay envelope in `crates/server/src/protocol.rs`
(`subscribe { scope }`, `event { event_id, scope, payload }`, `enroll_host`,
`run_report`, ...) is the **internal transport**, not the client protocol. It is
still captured here as `host-daemon.json` so the Node execution plane can be
checked in against it later, but loom is allowed to differ. This is the
"明确分歧" the issue asked for: the divergence is real and is confined to the
daemon half of the wire.

### What this changes for the current code

`crates/server/src/protocol.rs` currently serves both audiences on one `/ws`.
That is the thing to split, and it is follow-up work (this issue is export
tooling only):

- `/ws` becomes bb's client protocol. loom's `welcome`, `subscribed`,
  `event`, `host_enrolled` and friends move off it.
- daemon traffic moves to a daemon-only endpoint (bb uses `/internal/ws`).
- `/api/v1/*` is reserved for bb's routes. loom-native control endpoints
  (`/api/v1/publish`, `/api/v1/replay`, the current `/api/v1/version`) must move
  under a distinct prefix so they cannot collide with a route bb's UI expects.

Until that split lands, loom's `/ws` and `/api/v1/*` are knowingly divergent and
the contract tests will not cover them.

## Artifact format: JSON Schema, not OpenAPI

The export is **JSON Schema 2020-12 plus a manifest**, one file per surface.

Why not OpenAPI:

- Two of the four surfaces are not HTTP. bb's UI `/ws`, terminal `/ws` and the
  daemon `/ws` are WebSocket message protocols. OpenAPI cannot express them, so
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
messages (`validate_server_message`) and daemon frames
(`validate_daemon_message`, `validate_server_to_daemon_message`).
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
  such frame: a dispatch carries a prompt, nothing more. The daemon only ever
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
  receives one terminal event and the thread returns to `idle`. The daemon is
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
  answer to (because the run was stopped, or the daemon went away).

A settled interaction refuses a second answer with `409
awaiting_user_interaction`, which is a status the contract declares for that
code. `finish_run_with` settles every interaction a thread still had open when
its turn ended, so `hasPendingInteraction` in the thread list cannot be stuck.

**Where interactions come from.** ACP's `session/request_permission` is the
producer, wired in W-566. The daemon holds the request open and sends
`ClientCommand::InteractionRequest` up its socket; `AppState::record_interaction_request`
records a durable interaction and publishes `thread_interaction_changed` to the
thread scope; a client answers over the interaction routes; and
`AppState::deliver_interaction_resolution` publishes
`InteractionResolutionFrame` through the relay to `host:{id}`, where the daemon's
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
  a restarted daemon's fresh question from colliding with a settled row from an
  earlier run. The provider's own id stays verbatim in `origin`, so the answer
  frame can name it.
* **No client, no answer.** The daemon cancels an unanswered request after its
  permission timeout, cancels it immediately when the control plane refuses to
  record it, and cancels every open request when the connection drops. A
  cancellation is never an approval. See `docs/acp-adapter.md`.

`providerThreadId` on the interaction is the agent's own session id when it has
one, falling back to loom's thread id only when it does not.

### `threads.clearGoal` publishes an event; `threads.clearContext` refuses

loom has no goal entity. A goal is a projection of the thread's own run log: the
contract's `thread/goal/updated` and `thread/goal/cleared` events are the only
record, exactly as the reference client's `extractThreadTimelineGoal` treats
them. `threads.clearGoal` therefore publishes `thread/goal/cleared` and is
idempotent — the next `threads.timeline` read reports `goal: null`. Nothing was
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
are built by the same `timeline_row_for_event`, so a row returned here is
byte-identical to the same row in the timeline it came from. What differs is the
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
| `steer` | sent | `501 not_configured` |

`steer` is refused while busy because steering means injecting input into the
**running** turn, and `loom_provider_protocol` has no frame for that. Appending
the text as a second concurrent turn would be a different operation wearing the
same name. This is the same class of refusal as `threads.compact`.

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
| `steer` / `steer-if-active` | sent | `501 not_configured` |

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
