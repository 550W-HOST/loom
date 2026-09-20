# Automations

An automation is a **project-owned trigger** plus the execution it starts. It
belongs to exactly one project, fires on a cron `schedule` or a single `once`
instant, and runs either an agent turn or a script on the machine that owns the
workspace. Every attempt is a **run** row in the automation's history.

This page documents the delivery so far: the entities, their durable storage,
the typed HTTP surface (ten operations), the scheduler that fires them, the
agent execution that dispatches a turn, and the script execution that runs a
process on the machine that owns the automation. Realtime invalidation is the
stage that follows; what is *not* here yet is listed at the end, in as much
detail as what is.

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
| `AutomationExecution` | `agent { prompt, providerId, model, reasoningLevel, serviceTier, permissionMode, environment, targetThreadId }` or `script { script \| scriptFile, interpreter, timeoutMs, env, storedScriptPath }` |
| `AutomationRun` | id, automation, run mode, thread, state, trigger, `skipReason`, `error`, `output`, `exitCode`, `scheduledFor`, `startedAt`, `finishedAt` |
| `AutomationRunState` | `pending` → `running` → `succeeded` / `failed` / `skipped` / `cancelled`: the state this server stores, which is wider than the four the contract names |
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

**The write routes are strict at the edge.** bb's contract has no automations
entries, so `validate_contract_request` cannot see them; `automations_contract`
carries loom's own schemas — the same objects the conformance tests validate
responses against — and a middleware runs them on the way in. A key the
contract does not name is a `422 invalid_request`, at every level: the top-level
object, a `trigger`, an `environment`, the `workspace` inside it, a `branch`,
an agent `target`. That is not something serde can express on its own
(`deny_unknown_fields` does not reach an internally tagged union's unit
variant), and it must not be pushed into the stored row: storage stays tolerant
of a field a newer build added, because reporting a row as invalid stored data
is a worse outcome than reading it.

Failures use the existing vocabulary and never a new shape: `400
invalid_request` for a field the contract rejects, `404 not_found` for an
unknown project or automation, `409 conflict` for a stored row that cannot be
used as it is, and `422 invalid_request` when the body does not match the
contract at all (an unknown field, a bad enum), exactly as the contract routes
behave.

## Invalidation

A client renders automations from the routes above, and it is told to refetch
them the way it is told about every other change: a schema-checked `changed`
frame on the public socket, replayed through the relay, so a client that
reconnects does not have to know what it missed.

**The frame is the project's.** bb's public vocabulary has no automation entity
— automations were a plugin there, and their invalidation rode the plugin's own
realtime channel — while the pinned client's event names (`project:changed`)
and targets (`project-detail`, `project-list`) are fixed. So an automation or
run change is published as:

```json
{"type":"changed","entity":"project","id":"proj_…","changes":["project-updated"]}
```

That reaches exactly the clients that hold automations: the view for that
project subscribed with `project-detail`, and the workspace-wide overview
subscribed with `project-list` — list targets match by entity, detail targets by
id, and both are targets the pinned SDK already asks for. The frame says "this
project changed", not what changed: one frame covers an automation edit, a
pause, a queued run and a settled run, because a client cannot act on them
differently — each means refetch the automations it holds for this project.

**What publishes one.** Every mutation that a client could be rendering:

| producer | when |
| --- | --- |
| the write routes | after a successful create, update, delete, pause or resume is persisted |
| a manual run | after the run row exists — a deduplicated request publishes nothing, because nothing changed |
| the scheduler | for each project whose window it claimed, after the claim is on disk |
| a settle | a script report, a cancel, the reaper, a pre-dispatch failure, and a provider run's terminal event closing the agent run behind it |

Dispatch itself publishes nothing: a queued run and a dispatched one both report
`running`, so the transition is not a change a client could see.

Publishing is durable (`Scope::Global` in the relay, like settings) and happens
after the write, never before: a client told to refetch must find the run that
made it do so. A publish that fails resets public realtime rather than dropping
the frame silently, which is the same failure the socket already handles by
reconnecting and re-invalidating.

## The scheduler

A background sweep runs every `AppConfig::schedule_interval` (10 s by default,
`Duration::ZERO` disables it — which is what the tests use, so they drive
`AppState::sweep_automations` themselves). One pass does three things, in one
lock:

1. **Arms** an enabled schedule that has no `nextRunAt`. That is the state a
   row from before the scheduler is in, and the sweep is what brings it forward
   rather than leaving it inert.
2. **Claims** an automation whose window has arrived, *if* nothing of its is in
   flight. The claim queues a run row (`pending`, `trigger: schedule`,
   `scheduledFor` = the window) and advances the automation's `lastRunAt`,
   `runCount` and `lastRunStatus`.
3. Moves the schedule to its next occurrence **after now**. That single detail
   is what makes a restart harmless and a missed window harmless: the window
   that was claimed is behind `nextRunAt` before the snapshot is written, so a
   second sweep cannot see it as due again, and a server that was down for a
   week fires once and resumes its cadence instead of replaying a week.

A `once` trigger is spent by its claim: the automation disables itself rather
than firing a second time. An *evaluable* schedule with no remaining occurrence
(`0 0 30 2 *`) is claimed once and then disabled, because the alternative is a
promise the scheduler cannot keep. A schedule this build cannot evaluate — an
expression or a zone that does not resolve — is left exactly as it is and
reported in the sweep's counters, never fired blindly.

`SweepReport` carries those counters (`due`, `claimed`, `armed`, `in_flight`,
`unevaluable`, `unreadable`, `exhausted`) and the sweep logs them when anything
happened or when an automation is waiting on something. "Why did my automation
not fire" should be answerable from the log.

### The schedule engine

Two crates own the parts that are easy to get subtly wrong, and this is
deliberate: `croner` parses and evaluates the expression, `chrono-tz` supplies
the timezone database. The database is **compiled into the binary**, so a
scheduled run in a named zone works in the `FROM scratch` server image — there
is nothing to mount and no host `tzdata` to depend on.

The dialect is `croner`'s: `*`, lists, ranges, `*/S` and `X-Y/S` steps,
`MON`/`JAN`-style names, `?` in the two day fields, `L` and `#`. Two places it
differs from the reference plugin's evaluator, both asserted by tests: a bare
`N/S` step (`5/15`) is refused with croner's own message, and an empty list
entry (`1,,2`) is tolerated. The contract's own rule — exactly five
whitespace-separated fields — is enforced first, so a six-field expression is
rejected rather than reinterpreted.

DST is `croner`'s too, and pinned by tests here so a dependency upgrade that
changes it fails loudly:

* a local time the clock **skipped forward** over fires at the next instant that
  exists, so a daily schedule fires once that day instead of skipping it;
* a local time the clock **fell back** over fires once, on the earlier pass —
  the repeated hour does not produce a second run.

A zone name is resolved against the database when the trigger is written, so
"It is not a zone" is a `400` rather than a schedule that silently never fires.

## What is decided, and why

**A `run` request queues an execution intent; it does not execute anything.**
The response is the run row, and the contract has no `pending` state, so a
queued run reads as `running` with `scheduledFor = startedAt = now` and every
result field null. It stays in flight until the execution plane starts it and
reports a terminal state.

**Single-flight is one rule for both kinds of trigger.** A manual `run` and a due
window share it: a second request for an automation that already has work in
flight returns *that* run rather than starting a second one, and a due window
waits (its `nextRunAt` untouched) until the run in flight is finished. A
repeated `idempotencyKey` resolves to the run it created. A request that queued
a run answers `201`; one that was deduplicated answers `200` with the run it
resolved to, so a client can tell whether it started work without comparing ids.

**A run has five states; the contract names four.** `pending` and `cancelled`
have no contract spelling, so they project: `pending` → `running` (in flight
from a client's point of view) and `cancelled` → `skipped`, with the reason in
`skipReason`. The mapping lives in exactly one place
(`AutomationRunState::status`). Cancelling is what pausing does to work that had
not started — a queued run is abandoned when the automation is paused, because
leaving it behind would let the automation run once more after the user stopped
it. A run the execution plane has already started is not touched.

**Failure has a policy, not a guess.** A failed run increments
`consecutiveFailures`. A *scheduled* failure retries sooner than the next window
— 30 s, then 60 s, then 120 s, by setting `nextRunAt` — because a schedule that
just failed is the one thing a user wants to try again soon. A *manual* failure
never retries: nobody scheduled it, so there is nothing to retry. Any
non-failure resets the counter. The third consecutive failure disables the
automation, clears `nextRunAt` and appends "paused after N consecutive failures"
to `lastError` — a schedule that keeps failing is a broken automation, not a
busy one.

**`nextRunAt` is always an instant the scheduler will honour.** It is computed
in the trigger's zone, from the claim or the write instant, and it is `null`
only when there is genuinely nothing armed: a paused automation, a spent
one-shot, or a schedule that can never match again.

**Validation lives at the edge, with the contract's own limits.** Names ≤ 200
characters, script bodies ≤ 262 144 and script paths ≤ 200, cron expressions
≤ 100 (exactly five fields, each value in range, with lists, ranges, steps and
the three-letter month and day names), timezones ≤ 100 characters, idempotency
keys ≤ 200, script timeouts 1–900 000 ms (default 120 000), run pages 1–200
(default 50). A `once` instant in the past is rejected rather than converted
into an immediate run: "fire now" is what the manual operation is for. A script
execution must name exactly one of `script` and `scriptFile`.

**A host environment's `unmanaged` workspace always writes `path`.** The
contract spells that key as `z.string().min(1).nullable()` inside a `.strict()`
object, so the key is required and only its *value* may be null. The response
projection originally omitted the key whenever no path was set, which made an
automation created with an `unmanaged` workspace fail the contract's own
validation (`automationResponseSchema` → `workspaceArgsSchema` →
`$.execution` matches 0 `oneOf` branches). The key is now always written, as
`null` when there is no path. The gap survived the first conformance suite
because no test exercised a `host` environment; both projection shapes are now
asserted, in `crates/domain/src/automation.rs` and over HTTP in
`crates/server/tests/automations_conformance.rs`.

**`update` merges field by field.** A changed trigger re-arms the schedule only
while the automation is enabled (and clears `nextRunAt` when it is not); a
changed execution does not touch the schedule. `serviceTier: null` clears the
stored tier. An `agent` patch may only update an agent automation, and
`execution` and `agent` cannot be combined in one request. `pause` keeps the
failure state, clears the schedule and cancels queued runs; `resume` re-arms
from now (windows that passed while paused are not replayed) and clears
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
                       providerRunId, scheduledFor, startedAt, finishedAt }
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

The payload carries its own version. A version-1 payload — the release before
the scheduler — is migrated in place: a row stored as `running` was a queue
entry nothing could have claimed, and becomes `pending`. A version-0 payload
(written before the field existed) is upgraded the same way; a payload **newer**
than this build is not interpreted at all — the server starts with no automations rather than reading
another build's representation as its own, and says so on stderr. Duplicate ids
collapse to the first row, so a restored state is deterministic. Run rows have
no problem representation in the contract, so a run row this build cannot read
is left out of the listing; it stays in the snapshot verbatim and is not
deleted.

Writes are durable before the response: every successful mutation writes the
domain snapshot synchronously, the way a settings write does.

## Thread mapping

`AutomationThreadMark` is the durable half of "this thread was produced by that
run of that automation", written in the same critical section as the run's
`threadId` and `providerRunId` — the three facts are only ever true together.
Two consumers read it: the executor uses the run's own pair to open the thread
`automations_runs` returns, and `create` refuses an automation whose
`createdByThreadId` is itself an automation-produced thread — automation chains
that way have no bound, and the check is the same two-source lookup the
reference implementation uses (marks, plus the run history).

## Agent execution

A queued agent run becomes a turn through the **existing** path — there is no
second execution framework. The message is appended the way a client's message
is (which moves the thread to `working`), and `AppState::dispatch_thread`
resolves the environment, records the provider run and publishes the dispatch
to the owning host. The automation therefore inherits the whole lifecycle it
would otherwise have had to reimplement: the run registry, the pre-dispatch
failure timeline, provider reports, the thread status machine, permission
interactions, the deadline reaper.

`AppState::execute_pending_automation_runs` is the half of the loop that decides
*where* a run happens; the sweep calls it every tick, and a manual `run` request
calls it immediately so a client does not wait for the next tick.

### Which thread a run happens in

- **A declared `targetThreadId`** runs *in* that thread. It must exist, belong to
  the automation's project, and be `idle` or `error`; a thread that is working,
  waiting or archived fails the run with a reason (and, for a scheduled trigger,
  the ordinary retry). An automation states what it wants rather than joining
  someone else's queue.
- **Without a target**, each run gets its own thread in the automation's
  project, titled after the automation. One run, one conversation, and the run
  row carries the thread id.

### Which environment a run happens in

| declared | resolved to |
| --- | --- |
| `reuse { environmentId }` | that environment, unchanged (the dispatch preflight decides whether it is usable) |
| `host { workspace: unmanaged { path } }` | the project's unmanaged environment on that host with that path, created if it does not exist yet |
| anything else | **nothing** — the run fails, with a reason naming what to do |

The last row is the honest half. A `managed-worktree` or `personal` workspace has
to be *provisioned* on its host, and the environment entity carries no branch to
provision from, so "use the newest ready environment instead" would run the turn
in a workspace nobody asked for. `project-default` has the same problem: loom has
no server-side default-workspace resolution, which is exactly why a
`project-default` thread cannot dispatch either. All three fail visibly, with a
retryable reason.

The find-or-create rule for `unmanaged` is what keeps a schedule from growing an
environment per run: the second window finds the row the first one created.

### The run ↔ thread ↔ provider-run mapping

`AutomationRun` carries the thread it ran in and the provider run it became, and
`AutomationThreadMark` records the thread as automation-produced. The provider
run's terminal event is what ends the automation run: the hook is in
`settle_finished_thread`, the one place every terminal path passes through (a
report, a reaped run, a stop, a restart), so a run that timed out or whose host
went stale closes its automation run the same way a provider failure does. The
automation's own policy then applies: retry for a scheduled failure, pause after
three.

That mapping is also what makes a run's history navigable — the run response's
`threadId` is the conversation, and it is the same id the provider ran in.

### Around the edges

- **The provider is the server's.** An automation may name a `providerId`; a
  request for a provider this server does not run is logged and the turn is
  dispatched with the configured one. That is the same treatment a thread's
  recorded `model` gets, and for the same reason: the provider protocol carries
  no per-run model.

## Script execution

An agent run happens inside a provider; a script run happens **on a machine**.
The control plane never executes one: it names the owning host, hands the host
everything the process needs in one frame, and reads the report that comes back.
The transport is the same host-scoped relay room the host file and host RPC
channels use — `script.run` and `script.report` in `loom-provider-protocol` —
so there is no second delivery path to keep alive.

- **Which machine.** The primary host of the workspace the execution names. A
  script runs where the workspace *is*, so a project with no host of its own
  falls back to the personal host, and a workspace with none connected fails the
  run with "no connected machine holds automation …" rather than waiting.
- **Where the script lives.** In the script directory of that machine:
  `<data_dir>/automation-scripts/<automationId>/`. The control plane composes
  the path from the data directory the machine itself *reported* when it
  enrolled, and the worker creates the directory before running anything, so the
  layout stays the host's. An inline `script` is written there once, at first
  dispatch, and the automation then carries `storedScriptPath` — the file that
  actually ran, findable by a user. A `scriptFile` is resolved inside the same
  directory.
- **Path containment.** Two layers, both reused from the workspace rules: a
  lexical check that refuses an absolute path or any `..`, and a canonical one
  that resolves symlinks and refuses a target outside the directory. The refusal
  names the path and is what the run records.
- **What the process gets.** `cwd` is the script directory, stdin is null,
  stdout and stderr are captured, and the environment is *cleared* and rebuilt
  from `PATH`, whatever the automation declared in `env`, and the run's own
  identity (`LOOM_PROJECT_ID`, `LOOM_AUTOMATION_ID`, `LOOM_AUTOMATION_RUN_ID`,
  and `LOOM_SERVER_URL` — an injected child-script variable holding the URL the
  worker dialled with `--server-url`; none of these is operator configuration).
  Nothing else from the worker's environment reaches
  it — a script is a run of the automation, not of the worker.
- **Time.** `timeoutMs` (default 120 000, at most 900 000) is enforced on the
  machine. A script that outlives it is killed and the run fails with "Script
  timed out". Output collection is bounded by the same kind of budget: past it
  the run keeps draining the pipes — a process is never blocked on a full one —
  but stops collecting, and the run says so with a truncation marker instead of
  failing.
- **Output and exit.** A finished process closes the run: exit code 0 succeeds
  and keeps the output, any other code fails with "Script exited with code N",
  and anything the script wrote on stderr is appended to the output rather than
  hidden. Two cases from the reference are recorded as `skipped` rather than
  `succeeded`, because the run's meaning was its output: a script that printed
  only whitespace, and one whose last line is a `{"wakeAgent": false}` object.
  loom's script runs wake nothing — a script run has no thread — but such a
  script's history reads the way it did in bb, `exitCode` 0 and all.
- **Cancelling.** A running script is a process on a known machine, so pausing
  or deleting its automation stops it: the control plane publishes `script.cancel`
  to the host's room and settles the run in the same breath, because the user
  asked for it to stop and the host's report that arrives afterwards finds
  nothing in flight. The run's state is `cancelled` with the reason; the contract
  has one "not run" status, so the wire reports `skipped` with a `skipReason`.
  An agent run cannot be cancelled this way — the provider protocol has no frame
  for it — which is the difference the pause path encodes.
- **A machine that stops reporting.** A script run whose host disconnects is
  failed rather than left open: its process cannot be observed any more, and
  leaving the row in flight would block the automation's single-flight forever.

## Restart behaviour

The scheduler's state is the payload, so a restart is a resume:

- **A queued run survives.** It is durable work that has not started, and the
  sweep will not claim a second one while it waits.
- **A claimed window is not replayed.** Its `nextRunAt` moved past it before the
  snapshot was written, in the same critical section as the claim.
- **A run that was `running` when the process stopped is failed**, with "the
  server restarted while this run was in flight", and its automation takes the
  failure through the ordinary policy. That is the same argument the provider-run
  reconciler makes: no process can prove that in-flight work survived it, and
  leaving the row open would block the automation's single-flight forever. When
  the run had been dispatched, the failure arrives through the provider-run hook,
  so the automation and the thread agree about what happened.
- **A schedule from before the scheduler is armed** by the first sweep, so an
  upgrade does not leave every cron automation inert.

## Not here yet

- **Only two hosts have ever been exercised.** Script execution is verified
  against one server and one worker over real sockets — including a second
  worker identity that enrolls and is *not* chosen — but not against two
  machines at once, and not with a workspace whose primary host differs from the
  host that reported the data directory. The address of the script is derived
  per run from the workspace's primary host, so the gap is in coverage rather
  than in a known wrong path.
- **No script output on the wire while it runs.** Output is recorded on the run
  when the process ends; nothing streams it, and no relay event is written when
  a run settles (see realtime, below).
- **No on-demand environment provisioning.** A `managed-worktree`, `personal` or
  `project-default` execution fails rather than provisioning a workspace per
  run; bind a ready environment with `reuse` or name a host and an explicit
  path.
- **The invalidation is coarser than bb's was.** bb published two kinds
  (`automations-changed`, `automation-runs-changed`) on its plugin channel; the
  public protocol has one project frame, so a client refetches both. The
  alternative — a new entity or target — would need the pinned client to know a
  vocabulary it does not have.
- **No UI change.** The ported Automations view still talks to its typed seam;
  nothing here is wired to it yet.
- **No provider routing or permission resolution** against a provider catalog:
  the execution's provider, model, permission mode and environment are stored
  as given and validated for shape only.

## Tests

- `crates/domain/src/schedule.rs` — the cron dialect, zone resolution, and the
  DST cases: a skipped local time, a repeated one, a daily schedule across a
  transition, a weekday read in a zone rather than in UTC, and a schedule that
  can never match. These run against `chrono-tz`'s embedded database, so they
  are hermetic.
- `crates/domain/src/automation.rs` — trigger, execution, update, pause/resume,
  projection and limit validation, and the claim/outcome/arm policy.
- `crates/server/src/automations.rs` — row round trip, tolerant decode, read
  problems, version policy and the v1→v2 migration, deduplication on restore,
  delete cascade, single-flight (both directions), idempotent runs, the run
  state machine, the retry-then-pause policy, pause cancelling queued runs,
  restart recovery, cursors, ordering and project scoping.
- `crates/server/src/automation_execution.rs` — the executor: a queued agent
  run becomes a dispatched turn with a recorded mapping, the provider run's
  terminal event closes the automation run behind it, a run whose environment or
  host is missing fails with the reason *and* leaves the W-584 timeline on the
  thread, a target thread is reused and checked, an unmanaged workspace is
  created once, a script run with no machine or with a machine that never
  reported a data directory fails with the reason, a script run is dispatched to
  the machine that owns it carrying its script, workspace and timeout.
- `crates/worker/src/scripts.rs` — the runner in isolation: the interpreter an
  extension implies, names that cannot escape the directory, an inline script
  whose output and exit are reported, a non-zero exit reported verbatim, a
  script that outlives its timeout killed and said so, a cancel that kills the
  process, the worker's own environment excluded while a declared variable is
  visible, output over the budget truncated rather than failed, and both a
  traversal and a symlink out of the script directory refused.
- `crates/worker/tests/provider_e2e.rs` — the same paths over real sockets: a
  scheduled run and a manual run each become a turn whose thread carries the
  stub's output, the run record agrees with the thread about which conversation
  it was, a script automation runs on the worker and records its output, path and
  exit code, a non-zero exit fails the run with its code, a script that outlives
  its timeout is killed and reported, pausing the automation stops a running
  script and the run settles as cancelled, and a script path that leaves the
  script directory is refused by the host with the reason on the run.
- `crates/server/src/state.rs` (test support) — the public realtime channel is
  read the same way in every invalidation test: subscribe, decode the frame,
  assert it is the project's change.
- `crates/server/src/automations.rs` — the write routes publish one project
  scoped invalidation each (and a read publishes none), a deduplicated manual
  run publishes none, and a claimed window publishes for the project it belongs
  to.
- `crates/server/src/automation_execution.rs` — a settled script run, a
  cancelled one, a run that could not be dispatched, and a provider run ending
  the agent run behind it all publish the same frame.
- `crates/server/src/protocol.rs` — the frame is a legal project change, reaches
  both project targets, misses another project's, and passes the exported bb
  schema.
- `crates/server/tests/automations_conformance.rs` — every operation over HTTP,
  with request bodies validated against loom-authored schemas before they are
  sent and response bodies after they are read (through the same validator the
  bb contract routes use), plus the zone a schedule is armed in, a due window
  becoming a queued run, a script run with no machine failing with the reason
  while a pause still holds, a durable restart that neither
  replays a window nor loses a queued run, a payload from before the scheduler,
  a snapshot written before the field existed, a hand-damaged payload, and both
  `unmanaged` workspace projections (the `host` environment whose dropped `path`
  key the contract refuses).
