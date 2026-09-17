# Automations

An automation is a **project-owned trigger** plus the execution it starts. It
belongs to exactly one project, fires on a cron `schedule` or a single `once`
instant, and runs either an agent turn or a script on the machine that owns the
workspace. Every attempt is a **run** row in the automation's history.

This page documents the first delivery of that model: the entities, their
durable storage and the typed HTTP surface (ten operations). The scheduler, the
execution plane and realtime invalidation are the stages that follow; what this
stage does *not* do is listed at the end, in as much detail as what it does.

## Where the shape comes from

The contract is `ui/packages/automations/src/rpc-types.ts` — the pinned UI tree
imported by W-610 — and it stays the only source for it. Nothing here is
re-derived from the retired plugin runtime: the reference implementation's
*behaviour* (`plugins/automations/src/{data,service}.ts`) is followed where it
is visible on the wire (projection, read problems, run history, cursor), but
the plugin loader, its RPC transport and its marketplace stay gone.

## The entities

`loom-domain`'s `automation` module holds the pure types and their invariants:

| type | what it is |
| --- | --- |
| `Automation` | id, project, name, `enabled`, trigger, execution, origin, `createdByThreadId` and the run bookkeeping (`nextRunAt`, `lastRunAt`, `runCount`, `lastRunStatus`, `lastRunThreadId`, `lastError`) |
| `AutomationTrigger` | `schedule { cron, timezone }` or `once { runAt }` |
| `AutomationExecution` | `agent { prompt, providerId, model, reasoningLevel, serviceTier, permissionMode, environment, targetThreadId }` or `script { script \| scriptFile, interpreter, timeoutMs, env }` |
| `AutomationRun` | id, automation, run mode, thread, status, trigger, `skipReason`, `error`, `output`, `exitCode`, `scheduledFor`, `startedAt`, `finishedAt` |
| `AutomationThreadMark` | thread → (automation, run): the durable half of "which thread did an automation produce" |

Ids are loom's usual `auto_…` and `arun_…` ULIDs. `AutomationRunId` is a
different type from the provider `RunId` on purpose: an automation run is a
history row, a provider run is a dispatch.

## The HTTP surface

Ten operations, scoped by project and automation id. The contract's input
objects split across the two places that carry them: the ids are **path
parameters**, everything else is the request body (or the query string for a
read). Nothing travels twice.

```text
GET    /api/v1/automations                                     → { automations: [{ automation, project: { id, name } }] }
GET    /api/v1/projects/{projectId}/automations                 → [automationReadResult, …]
POST   /api/v1/projects/{projectId}/automations                 { name, enabled?, trigger, execution, origin, createdByThreadId? } → 201 automationResponse
GET    /api/v1/projects/{projectId}/automations/{automationId}  → automationReadResult
PATCH  /api/v1/projects/{projectId}/automations/{automationId}  { name?, trigger?, execution?, agent? } → automationResponse
DELETE /api/v1/projects/{projectId}/automations/{automationId}  → { ok: true }
POST   /api/v1/projects/{projectId}/automations/{automationId}/pause  → automationResponse
POST   /api/v1/projects/{projectId}/automations/{automationId}/resume → automationResponse
POST   /api/v1/projects/{projectId}/automations/{automationId}/run    { idempotencyKey? } → 201 { run }, or 200 when deduplicated
GET    /api/v1/projects/{projectId}/automations/{automationId}/runs   ?limit=&cursor= → { runs, nextCursor }
```

These routes are **contract-external**: bb's exported contract has no
automations entries, so they are listed as loom-native product routes in
[`api-coverage.md`](api-coverage.md) and are not counted in the bb coverage
number. They are not "missing" from anything.

Failures use the existing vocabulary and never a new shape: `400
invalid_request` for a field the contract rejects, `404 not_found` for an
unknown project or automation, `409 conflict` for a stored row that cannot be
used as it is, and the framework's `422 invalid_request` when the body does not
match the request shape at all (an unknown field, a bad enum), exactly as the
contract routes behave.

## What is decided, and why

**A manual `run` records a run; it does not execute anything.** The response is
the run row, `running`, with `scheduledFor = startedAt = now` and every result
field null. Nothing in this stage may move it to a terminal status, so it stays
in flight until the execution plane reports one — which also means the
single-flight rule below is observable immediately: a second `run` for an
automation that already has a run in flight returns *that* run rather than
starting a second one. A repeated `idempotencyKey` does the same, and that is
the whole of the deduplication. A request that created a run answers `201`; one
that was deduplicated answers `200` with the run it resolved to, so a client can
tell whether it started work without comparing ids.

**`nextRunAt` is only set where the answer is exact.** A `once` trigger is its
own answer, so an enabled one-shot reports its instant; a cron `schedule`
reports `null`, because computing the next occurrence needs the timezone-aware
scheduler this stage deliberately does not contain. An absent field is honest;
a plausible-looking number the server would not honour is not.

**Validation lives at the edge, with the contract's own limits.** Names ≤ 200
characters, script bodies ≤ 262 144 and script paths ≤ 200, cron expressions
≤ 100 (exactly five fields, each value in range, with lists, ranges, steps and
the three-letter month and day names), timezones ≤ 100 characters, idempotency
keys ≤ 200, script timeouts 1–900 000 ms (default 120 000), run pages 1–200
(default 50). A `once` instant in the past is rejected rather than converted
into an immediate run: "fire now" is what the manual operation is for. A script
execution must name exactly one of `script` and `scriptFile`.

**Timezone validation is a shape check.** Resolving a zone to an offset needs a
timezone database, and there is no scheduler to consume the answer yet; an
unknown-but-well-shaped name is therefore accepted here and will be rejected by
the scheduler stage, where the database lives. This is the one validation in
this stage that is deliberately weaker than the reference implementation.

**`update` merges field by field.** A changed trigger re-arms the schedule only
while the automation is enabled (and clears `nextRunAt` when it is not); a
changed execution does not touch the schedule. `serviceTier: null` clears the
stored tier. An `agent` patch may only update an agent automation, and
`execution` and `agent` cannot be combined in one request. `pause` keeps the
failure state and clears the schedule; `resume` re-arms it and clears
`lastError` and `consecutiveFailures`.

**Delete takes the history with it.** The automation's runs and thread marks
are removed in the same operation, so a later run listing cannot resolve an
automation that no longer exists.

## Stored rows, and what a damaged one does

The snapshot stores rows, not decoded values (see
[`domain-persistence.md`](domain-persistence.md)):

```text
StoredAutomation  { id, projectId, name, enabled, triggerType, trigger, runMode,
                    execution, origin, createdByThreadId, nextRunAt, lastRunAt,
                    runCount, consecutiveFailures, lastRunStatus,
                    lastRunThreadId, lastError, createdAt, updatedAt }
StoredAutomationRun  { id, automationId, runMode, threadId, status, trigger,
                       skipReason, error, output, exitCode, idempotencyKey,
                       scheduledFor, startedAt, finishedAt }
StoredAutomationThreadMark  { threadId, automationId, runId, createdAt }
```

Every field defaults, so any JSON object deserializes; `trigger` and
`execution` are kept as raw JSON, and `triggerType`/`runMode` are the stored
discriminators that must agree with them. Reading a row is a separate, fallible
step, which is what makes the contract's read union meaningful:

- a readable row is the automation;
- a row whose agent prompt is empty — the shape a build that allowed it wrote —
  reads as `missing-agent-prompt` and is refused by every write except the one
  that supplies a prompt;
- a row that does not decode, or whose stored discriminator contradicts its
  JSON, reads as `invalid-stored-data`: it is reported with its id, project and
  name so a client can offer to delete it, and it is **not** dropped, repaired
  by guesswork, or made the caller's problem as a 500.

The payload carries its own version. A version-0 payload (written before the
field existed) is upgraded in place; a payload **newer** than this build is not
interpreted at all — the server starts with no automations rather than reading
another build's representation as its own, and says so on stderr. Duplicate ids
collapse to the first row, so a restored state is deterministic. Run rows have
no problem representation in the contract, so a run row this build cannot read
is left out of the listing; it stays in the snapshot verbatim and is not
deleted.

Writes are durable before the response: every successful mutation writes the
domain snapshot synchronously, the way a settings write does.

## Thread mapping

`AutomationThreadMark` is the durable half of "this thread was produced by that
run of that automation". The execution stage writes marks when it spawns a
thread; the consumer that exists today is `create`, which refuses an automation
whose `createdByThreadId` is itself an automation-produced thread — automation
chains that way have no bound, and the check is the same two-source lookup the
reference implementation uses (marks, plus the run history).

## Not in this stage

- **No scheduler and no sweep.** Nothing fires on its own; `once` and `schedule`
  are validated and stored, and `nextRunAt` stays `null` for cron.
- **No execution.** No provider dispatch, no script on a host, no script files
  on disk. `storedScriptPath` is therefore never emitted, and an inline script
  is stored as it was given.
- **No terminal run status.** A run this stage creates stays `running`.
- **No realtime invalidation.** Mutations write no relay event, so no client is
  told to refetch; the routes are the only way to observe a change.
- **No UI change.** The ported Automations view still talks to its typed seam;
  nothing here is wired to it yet.
- **No provider routing or permission resolution** against a provider catalog:
  the execution's provider, model, permission mode and environment are stored
  as given and validated for shape only.

## Tests

- `crates/domain/src/automation.rs` — trigger, execution, update, pause/resume,
  projection and limit validation, as unit tests.
- `crates/server/src/automations.rs` — row round trip, tolerant decode, read
  problems, version policy, deduplication on restore, delete cascade,
  single-flight and idempotent runs, cursors, ordering and project scoping.
- `crates/server/tests/automations_conformance.rs` — every operation over HTTP,
  with request bodies validated against loom-authored schemas before they are
  sent and response bodies after they are read (through the same validator the
  bb contract routes use), plus a durable restart, a snapshot written before
  the field existed, and a hand-damaged payload.
