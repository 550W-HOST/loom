# Handoff

State of the work, what is verified, and what is next. Written for whoever picks
this up — including a future session with no memory of the reasoning.

Everything below was measured in this checkout on 2026-09-13 unless a line says
otherwise. Where something could not be confirmed, it says so rather than
guessing.

## Where things stand

| | |
| --- | --- |
| Branch | `main`, in sync with `origin/main` (`f3a1c15`) |
| Working tree | clean |
| Tests | **624 passing**, 0 failing (`cargo test --workspace --locked`) |
| Route coverage | **105 / 149** (70.5%) — `docs/api-coverage.md` |
| Crates | `relay`, `relay-hub`, `server`, `worker`, `domain`, `provider-protocol`, `contract` |
| UI | `ui/` workspace, 18 tests, typecheck clean |
| CI | fmt + clippy `-D warnings` + test + MSRV + contract reproducibility + UI + pi |
| Test count history | 253 → 416 (B2) → 461 (B3) → … → **624** (B7) |

## Unverified: a commit I cannot find

`d7fbf72` was mentioned as a documentation commit that needs an amendment. **It
does not exist.** Checked in this checkout and against the remotes:

- `bb`: not in the object database, no reflog entry, no dangling object, not in
  any local or remote branch, not among the `refs/pull/*` heads
- `pi-acp`: not present
- The GitHub API returns 404 for `550W-HOST/loom` and 422 ("No commit found for
  SHA") for `AndPuQing/pi-acp`
- `git fetch origin d7fbf72` fails on both: "couldn't find remote ref"
- Every git repository under `/data/workspace/ljm/dev/`
- Every multica workspace checkout under `/home/ljm/multica_workspaces/`

The newest commit on `origin/main` is `f3a1c15` (2026-09-13 02:25). The most
recent docs commit is `7be3f72` ("Document the provider strategy"), written in
the previous session.

Conclusion: **the hash is stale or invented — there is no such commit to
amend.** It is not a case of sitting in a checkout this environment cannot see,
because GitHub itself has no record of it. The documentation problems that were
reported alongside it turned out to be real, and are fixed below.

## Real documentation defects, now fixed

Three were found. Two were inherited; the third (`provider-strategy.md`'s ACP
version recommendation) was mine, and it would have blocked the migration.
They are kept on the record because the *pattern* is worth watching: decisions
recorded from schema reading alone, without cross-checking the implementation
that has to satisfy them.

### 1. `docs/provider-sessions-research.md` was stale — fixed

The previous session wrote this doc, then wrote `docs/provider-strategy.md` and
changed the plan underneath it. The research doc still presented per-agent
session-file reading as a live option, including a `--session-id` conflict that
no longer exists, while the strategy doc had already decided
"**loom never reads an agent's session files**" and "ACP only".

Fixed by marking the document superseded in its conclusions while keeping its
measurements, which nothing else covers. The three design questions now record
how each was decided and why the rejected options were rejected; the
file-partitioned branch of its design diagram is shown as the branch that was
deleted rather than implemented. The W-558 paragraph now says the issue is
parked and why.

### 2. `docs/provider-strategy.md` named the wrong trait bound — fixed

The "Pi is embedded, not spawned" section and migration step 2 both said the
required `pi-acp` entry point takes `impl ConnectTo<Client>`. That is what I
guessed in the issue and what the agent corrected during implementation:

> The bound is `ConnectTo<Agent>`, not `ConnectTo<Client>` as the issue
> sketched: in agent-client-protocol 2.0.0 `ConnectTo<R>` is the *counterpart*
> role, so the SDK's own agent-side entry point is
> `AgentProtocolRouter::connect_to(client: impl ConnectTo<Agent>)`.

Both places now say `ConnectTo<Agent>` — verified with
`grep -n 'ConnectTo<' docs/provider-strategy.md` — and the section notes where
the correction came from. Migration step 2 also now states that the `pi-acp`
half is done and tested, rather than "requires the entry point tracked in the
`pi-acp` project".

### 3. `docs/provider-strategy.md` recommended ACP v2 — wrong, fixed

This one was mine and it was the most consequential. The document recorded
"**ACP version | v2.** v1 is refused rather than degraded." I had read the v2
schema, seen that it covers the render surface better, and recommended it —
**without checking what `pi-acp` actually speaks**, which was the entire point
of the exercise. The handoff doc then repeated it as settled.

Three facts make the original decision unimplementable:

1. **v2 is an unstable draft.** It lives behind
   `#[cfg(feature = "unstable_protocol_v2")]` (`schema/src/lib.rs:44`), and
   without the feature `LATEST` resolves to v1 (`schema/src/version.rs:49`).
2. **v2 has no `session/load`.** It was replaced by `session/resume`. The method
   `loom resume` was designed around does not exist in v2.
3. **`pi-acp` hardcodes a v1 reply** (`pi-acp/src/agent.rs:524-527`): it logs the
   requested version and answers `ProtocolVersion::V1` regardless. A client that
   "refuses v1" therefore refuses to talk to `pi-acp` at all — the adapter this
   project had just finished wiring for embedding.

A fourth thing, less severe but instructive: **`SessionUpdate::Other` does not
exist in v1** (`grep -c Other schema/src/v1/client.rs` → 0), so the
"store + log, don't render" decision recorded for it was inert as written. Under
v1 the interception point is the raw JSON-RPC layer (`UntypedMessage`).

Corrected to **negotiate**: v2 first, v1 on the same connection, via the SDK's
owner `Client::protocol_connector().with_v1(..).with_v2(..)`. The document now
says why this is *not* the fallback the no-fallback rule forbids (v1 is what the
ecosystem speaks and the only version with `session/load`), records the one
lossy conversion edge (`CurrentModeUpdate`, v1-only, skipped on the v2 path),
and notes that the SDK converts at `initialize` but pipes frames unconverted
afterwards.

Supporting both versions on the `pi-acp` side is tracked as **W-562**.

**The generalisable lesson**: every "already decided" row in these docs should
be treated as verified only if it was checked against the sibling
implementation, not merely against a schema. This row was decided from schema
reading alone, and it was wrong in a way that would have blocked the migration.

## pi-acp: W-559 and W-562 both done and verified

W-559 in the `pi-acp Rust 重写` project (`166a0b99`), assigned to
`全栈开发者-pi`, status **done** (2026-09-12 16:11). Commits:

```
828d3ed Merge pull request #32
4d8e9cf style: apply rustfmt to the pre-existing drift blocking CI (W-559)
b38c165 feat(agent): add transport-injectable run_with entry (W-559)
```

What shipped:

- `AcpAgent::run_with(client: impl ConnectTo<Agent> + 'static)` carries the
  whole builder chain, unchanged except that it connects to the injected
  transport instead of `Stdio::new()`
- `run()` keeps its exact signature and behaviour, and is now literally
  `self.run_with(Stdio::new()).await` — the binary and Zed need no change
- Only semantic change anywhere: the error label `"acp-stdio"` became the
  transport-neutral `"acp-transport"`
- New test `crates/pi-acp/tests/acp_in_process.rs` (217 lines) drives the agent
  in-process over `Channel::duplex()` against a mock pi: `initialize` →
  `session/new` → `session/prompt` → streamed `SessionUpdate` → `EndTurn`. An
  `#[ignore]`d variant runs against real pi

**Independently verified here** (pulled `pi-acp` to `828d3ed`, ran the test):

```
running 2 tests
test in_process_against_real_pi ... ignored, requires a real pi binary with configured auth
test in_process_channel_against_mock_pi ... ok
test result: ok. 1 passed; 0 failed; 1 ignored
```

So the embedded-Pi architecture in `docs/provider-strategy.md` is buildable, not
just plausible.

**Note**: the local `pi-acp` checkout was behind. `git pull` was needed before
the test existed. Anyone verifying this must pull first.

### W-562 (v2 support): done at `b3e7e8f`, verified here

Status **done** (2026-09-13 11:22). 2,431 insertions across 16 files, including
741 lines of v2 tests and two new modules (`src/v2.rs`, `src/protocol.rs`).

Verified independently by running both configurations:

```
default (feature off):  194 passed, 0 failed
default + protocol-v2:   196 passed, 0 failed
```

The first attempt at the feature-enabled run failed with `could not compile
foldhash` — that was the known sccache fault (a stale
`/tmp/multica-task-*/sccache*` temp dir), not a code problem.
`sccache --stop-server` cleared it.

Checked against the issue's acceptance criteria rather than trusting the green
tests:

| Requirement | Verified |
| --- | --- |
| Feature off by default, default behaviour unchanged | yes — no `default` key in `[features]`, and the default test suite is green |
| `initialize` answers the requested version | yes — `agent.rs:651-665`, feature-gated; the old hardcoded `V1` is gone |
| Uses `AgentProtocolRouter` | yes — `agent.rs:537-539` `.protocol_router().with_v1(v1).with_v2(..)` |
| One conversion at the boundary, no unconverted frame | yes — `protocol.rs:109` `send_session_update` is the single outbound point, and a conversion failure surfaces rather than dropping |
| `message_id` minted at `message_start` | yes — `session.rs:1500`, `pi-msg-<n>`, session-scoped counter |
| Patch object on v2 only | yes — `OutboundMessage::AgentMessage` → `send_agent_message`; v2 clients get it, v1 stays chunk-only |
| `CurrentModeUpdate` skipped on v2 | yes — `protocol.rs:129`, with the reason in a comment just above |
| v2 completion via `state_update` | yes — `foreground_state_notification`, v2-only |
| `resume`: `start` replays, omitted does not | yes — `plan_resume` (`v2.rs:317`), asserted by two tests |
| Unknown replay cursor rejected | yes — neither `_`-prefixed nor future variants are guessed at; asserted by a test that also checks the error names the cursor |

One thing the agent did beyond the issue, which matters: **v2 reports turn
completion through `state_update`, not `PromptResponse`.** I had not accounted
for this — v2's `PromptResponse` carries no `stopReason`. Since loom's whole
run lifecycle depends on exactly one terminal event per run, this was
load-bearing and would have been a bug had it been missed.

## B3 verification (W-557)

W-557 is **done** (`1f2f865`, merge `f3a1c15`). Its first four runs failed — one
context overflow at 211,878 tokens against a 200,192 limit, one 530 from the
backend, two cancelled. The fifth attempt succeeded. Worth knowing because the
failure mode is "issue looks stuck", not "issue looks wrong".

Independently verified by building release binaries, running a real server and a
real worker, and calling all fourteen routes by hand. Results:

| Route | Result |
| --- | --- |
| `GET /queued-messages` | 200 |
| `GET /threads/:id/queued-messages` | 200 |
| `GET /threads/:id/interactions` | 200 |
| `GET /threads/:id/interactions/:id` | 404 (none exist — see below) |
| `GET /threads/:id/events/wait` | 200 `null` on timeout |
| `GET /threads/:id/timeline/turn-summary-details` | 200 with data |
| `POST /threads/:id/queued-messages` | 201 |
| `POST .../queued-messages/:qmid/send` | 409 (`queued_message_claim_lost`) |
| `POST .../interactions/:id/cancel` | 404 |
| `POST .../interactions/:id/resolve` | 422 (body shape) |
| `POST .../interactions/:id/respond` | 422 (body shape) |
| `POST /threads/:id/plan/cancel` | 501 `not_configured` |
| `POST /threads/:id/context/clear` | 501 `not_configured` |
| `POST /threads/:id/goal/clear` | 200 `{"ok":true}` |

The refusals are the no-fallback rule working as documented: `plan/cancel` and
`context/clear` say exactly what loom cannot do instead of returning a
plausible-looking success.

The 409 on `send` is real semantics, not a defect: the auto-drain had already
delivered the message, so the manual send lost the claim. Correct behaviour, and
it proves the claim mechanism is not a stub.

The 422s were verified to be *correct* — request bodies were rejected with JSON
Pointer-precise messages (`missing required property mode`,
`value "queue" is not one of the allowed values`) until the body matched
`{"mode":"auto"}`.

### The gap: nothing in loom produces an interaction (the source exists upstream)

`record_interaction` and `create_interaction` have **no callers outside tests**.
Verified in this repository:

```
$ grep -rn "record_interaction\|create_interaction" crates/ --include=*.rs | grep -v "/tests/"
crates/server/src/interactions.rs:63:    pub fn record_interaction(
crates/server/src/interactions.rs:75:        let (interaction, event) = self.registry.create_interaction(
crates/server/src/domain_state.rs:914:    pub fn create_interaction(
```

And loom's provider protocol cannot express one either — `provider-protocol` has
`ProviderSpec`, `RunDispatch`, `EnvironmentProvision`,
`EnvironmentProvisionOutcome`, `EnvironmentProvisionReport`, `ProviderReport`,
`GuardedLine`, and no interaction frame. So the five interaction routes are
implemented, contract-shaped, persisted and tested, but **loom never creates an
interaction to serve**.

What I got wrong when this was first written: I recorded that ACP would supply
the producer, as if it were future work. **It already exists, and loom is
actively suppressing it.** `crates/worker/src/provider.rs` declines these
requests on the Pi path:

```rust
// A dialog request blocks the provider until answered. There is no UI
// on this path yet, so decline it explicitly rather than hang, and tell
// the client what was declined.
if let Some(response) = auto_cancel_response(&frame) {
    ...
}
```

So the producer is not missing from the system — it is reachable and
deliberately answered with `cancelled`. Three consequences worth stating:

1. **The `auto_cancel_response` path is a real, exercised producer** and its
   refusals are a product decision ("no UI on this path yet"), not an accident.
   Once interactions have somewhere to render, this is the call site that should
   create them instead of cancelling.
2. **`pi-acp` already maps pi's dialog requests onto real ACP permission
   requests.** `handle_extension_ui_request` turns pi's `select` into
   `session/request_permission` with one `PermissionOption` per choice, and
   `confirm` into Yes/No options, with the answer flowing back to pi
   (`session/session.rs:2791`, `:2883`). So the ACP adapter path does not need a
   producer built — it needs loom to interpret the request it already receives.
3. The interaction gap is therefore **a UI/projection gap, not a protocol gap**.
   That reframes the work: it belongs with the front end, not with the provider
   migration.

Same shape of gap for two neighbouring things, though the details differ:

- `ProviderEvent::ThreadGoalUpdated` — **no producer at all.** A goal is a
  projection of the run log (`crates/server/src/http.rs:3706` explains the
  design), and nothing ever writes a goal into that log. Its counterpart,
  `ThreadGoalCleared`, *does* have exactly one producer: the `goal/clear` route
  itself publishes it. So `goal/clear` clears a projection that nothing sets —
  idempotent and honest, but not yet useful.
- `ProviderEvent::Plan`, `PlanSteps`, `TurnPlanUpdated`, `ItemPlanDelta` — same
  as `ThreadGoalUpdated`: defined, mapped to contract event names, never
  constructed outside tests. Note that **`pi-acp` emits no plan updates at all**
  (verified: no `SessionUpdate::Plan*` anywhere in its source), so this one
  genuinely has no upstream source on the Pi path yet.

## Route batches

| Batch | Theme | Routes | Status |
| --- | --- | ---: | --- |
| B0 | Baseline | 53 | done |
| B1 | Startup, navigation, first threads flow | 14 | done (W-548) |
| B2 | Thread control and auxiliary views | 14 | done (W-556) |
| B3 | Interactions, plans, queue sending | 14 | done (W-557) |
| B4 | Thread lifecycle and queue management | 14 | done (W-568) |
| B5 | Thread files and storage helpers | 10 | done (W-565) |
| B6 | Environment lifecycle and repo status | 14 | done (W-573) |
| B7 | Project workspace, attachments, sections | 14 | done (W-569) |
| B8 | Host and environment connectivity | 14 | |
| B9 | Files and terminals | 17 | |
| B10 | Settings and system preferences | 13 | |

B5 covers thread counts, pane actions, and host file / thread storage reads. It
depends on B1 and B4, both of which are done. Its one protocol addition is
`HostFileRequest` / `HostFileReport`: a file read is a request to the thread's
host, published through the relay, so the control plane never reads its own disk
and calls the result a thread's file. See `docs/contract.md` ("B5") for the
permission scopes and the two-half traversal defence.

B7 applies the same rule to a project: its workspace files are read from the
project's source host, and an upload is a `HostFileOperation::Write` the worker
confines to the host's own `project-attachments/<project_id>` directory. It also
made `Project` orderable (`sort_key`), `Project` deletable (a tombstone) and
`ThreadSection` a real entity; see `docs/contract.md` ("B7").

**Before assigning B4**, note that file overlap is what actually causes merge
pain — each multica task gets its own worktree and conflicts surface only at
merge. B1/B2/B3 all touch `crates/server/src/http.rs` (the B3 commit alone adds
1,568 lines there) and `docs/api-coverage.md`. Batching 14 routes per issue is
what caused W-557 to blow the 200k context limit at least once. Consider
splitting a batch into two issues of 7 if the routes touch disjoint modules.

`W-541` is the parent tracker for all batches and is still `todo`; it closes when
B10 does.

## The ACP migration: state

`docs/provider-strategy.md` is the decision record. The decisions:

| Question | Decision |
| --- | --- |
| One protocol or several? | **ACP only.** Pi is not special-cased at the client. |
| How is Pi reached? | **`pi-acp` embedded as a library**, over `Channel::duplex()`. |
| ACP version | **Negotiated: v2 first, v1 fallback.** loom offers both through the SDK's protocol connector, which starts v2 and restarts on v1 for an agent that answers v1. `pi-acp` v0.5.0 serves v2 natively, so the Pi path is v2. |
| Resume entry point | The next run carries the stored provider session id and resumes it: `session/resume` under v2, `session/load` under v1. |
| Unsupported capability | **Reported, never worked around.** |
| Unmapped update type | An unmapped v1 update is ignored by the typed schema and logged by the adapter; no synthetic event is emitted. |

loom now depends on `pi-acp` and the ACP SDK. `ProviderLaunch` has only two
ACP forms: `AcpEmbeddedPi` for Pi and `AcpStdio` for native agents. The old
`effective_argv`/`--session-dir`/`--session-id` path and direct Pi JSON-RPC
mapper have been removed.

The current ACP v1 flow is:

```text
first run:  session/new → returned sessionId → thread/identity → persist id
next run:   dispatch id → session/load(id, cwd) → suppress history replay → prompt
```

The server stores the opaque id with the thread and includes it in the next
`RunDispatch`; loom never reads an agent session file. The adapter serializes
construction/report ordering, checks `loadSession` before resuming, and treats a
missing workspace or unsupported restore as an explicit run failure. A real
second-run regression test is in `crates/worker/tests/acp_session.rs`.

The ACP v2 schema and negotiation are not enabled in this checkout yet. The
stable v1 path is deliberate: it is the default protocol implemented by the
pinned `pi-acp` dependency.

Open questions carried forward:

- Does every target agent implement `session/list`? It is a capability, so no —
  the import flow must omit rather than guess.
- Version policy for the `agent-client-protocol` crate (distinct axis from the
  protocol version).
- Does the embedded `pi-acp` need process isolation? A panic in the translator
  would take the worker with it; a spawned process would not.

## Environment notes

- **Superseded by the CLI migration (after the date above).** Configuration is
  flags now, not environment variables. server: `--bind` is the listen address,
  default `127.0.0.1:38886`; the others are `--data-dir`, `--node-id`,
  `--redis-url`, `--ui-proxy` and `--artifact-dir`. worker: `--server-url`,
  `--name`, `--state` and `--auto-update`/`--no-auto-update`, with the rest in
  `crates/worker/src/cli.rs`. The only env fallback left is `LOOM_JOIN_CODE`
  (for `--join-code`); `LOOM_REDIS_URL` survives only as a removed tombstone
  that fails at startup, and the `LOOM_*`
  configuration variables this section originally named are no longer read.
- A stale `target/debug/loom server` from an earlier session held the default
  port. Check `ss -tln` before assuming a startup failure is a code problem
- `sccache` occasionally fails with `exit status: 254`; `sccache --stop-server`
  clears it. Not a loom issue
- aarch64 musl cross-compilation needs `cargo-zigbuild` or `cross`; this machine
  has docker but no buildx. Without buildx, container images can still be
  reproduced with `docker build --build-arg TARGETARCH=amd64` after staging a
  binary and touching `.keep`

## multica operations that were used

```
multica issue assign <id> --to "全栈开发者-pi"     # agents: -pi / -omp / -openai / -pi-ds
multica issue runs <key> --output json           # run status, error, workdir
multica issue get <key> --output json            # includes description, identifier
multica issue children <key>                     # batch issues under a tracker
multica issue status <key> done
multica project resource list <project-id>       # which repo a project points at
```

Gotchas found:

- `issue list --project <id> --output json` caps at 50 and does not return
  `identifier`; `issue get` on the key or id does. Use `get` when the key
  matters
- The loom project is `5b8f4567-2a2e-4c20-8cd4-1c59689684f8`; the pi-acp project
  is `166a0b99-aab5-415a-a1d0-00bf22052804`
- multica squash-merges agent branches into `main`; `git fetch --prune` after

## pi-acp upgraded to `v0.5.0` (2026-09-21)

The pin moved from `branch = "main"` @`2f13a18` to `tag = "v0.5.0"` @`5adb199`,
and the manifest's `features = ["protocol-v2"]` was deleted: that release
**removed the feature** and now always compiles both protocol implementations,
selecting one per connection in `initialize`.

Measured on this checkout: `cargo check --workspace --locked` clean;
`cargo test --workspace --locked` **1053 passed / 0 failed**; `real_pi`'s three
`#[ignore]`d tests pass against the embedded adapter, which negotiates **native
ACP v2** — `LOOM_ACP_TRACE=1` prints `negotiated ACP v2`, and the frames carry
v2 `messageId`s. The v2 shapes were checked field by field against `render.rs`:
nothing loom needs is dropped, and the bash terminal still travels in
`_meta.terminal_output` / `_meta.terminal_exit`. (`pi-acp`'s own comment at
`session.rs:336-343` claiming it moved to `terminal_update` is stale — the
renderer streams `_meta`, and loom reads it correctly.)

Two traps found on the way:

- **The ACP real-agent tests are not pinned to the adapter they think they
  test.** `pi_acp_binary()` prefers `../pi-acp/target/release/pi-acp`, which
  reports `v0.1.0`; the sibling's debug binary reports `v0.4.0`. Those suites
  only exercise the pinned version when `PI_ACP_BIN` names a fresh build.
- **`session/list` against a real `~/.pi/agent/sessions` is slow by
  construction.** `pi-acp` re-runs its full session scan once per page
  (`LIST_PAGE_SIZE` = 50); with 305 files / 132 MB that is seven scans, which
  measured ~111 s against a 30 s probe budget. The capability probe now points
  the adapter at an empty agent directory (`PI_CODING_AGENT_DIR`), so it takes
  the identical `Listed` path in ~0.03 s. The upstream inefficiency is **not
  fixed**; note also that `list_sessions` has no production caller yet, so only
  tests reach it.

## pi-acp upgraded to `v0.5.1` (2026-09-22)

`v0.5.0` shipped a regression with its native v2 bash renderer. A bash call is
opened by the streamed `toolcall_start`, which carries the tool's name and not
yet its arguments, so the opening frame can only name the call `"bash"`; the
command arrives one frame later on `tool_execution_start`. `bash_v2_frames`
named the call on `first` alone, so that later frame sent status and `_meta`
only and the command never reached the client — every bash row in the timeline
showed `bash` with its command gone. v1 had the same regression against its
pre-native-split shape, which re-stated `kind`/`title` on every update.

`v0.5.1` (`pi-acp` `081fb63`, `ccf1b43c`) states the name on every frame: v2's
patch semantics make that the intended mechanism (an omitted field leaves the
previous value, a concrete one replaces it). loom needed no code change — it
already applies every `tool_call_update`, so the corrected `title`/`rawInput`
now rebuild the `CommandExecution` with its command. The fix's own evidence is
pi-acp's `v2_bash_call_is_named_with_its_command_after_the_streamed_open`.

Measured on this checkout: `cargo check --workspace --locked` clean;
`cargo test -p loom-worker --lib acp --locked` **68 passed**.

## pi-acp upgraded to `v0.5.2` (2026-09-22)

`v0.5.0`'s settle fallback was a wall clock from prompt acceptance that nothing
refreshed, so any turn that outlived `PI_ACP_SETTLE_TIMEOUT_SECS` (600 s) was
failed with `settleTimeout` — a long bash build died at ten minutes even while
pi was streaming its output. loom's own bounds are 30 minutes
(`DEFAULT_RUN_TIMEOUT`, and the server's `run_timeout`), so the adapter's nested
deadline fired first and reported a different error for a healthy run.

`v0.5.2` (`pi-acp` `9544ac3`, `dcd83668`) bounds *silence* instead: every event
pi sends re-arms the deadline, and a tool that has started and not ended holds
it off, so only a prompt that never gets going is failed — the risk #84 case the
fallback was built for.

loom now states the budget instead of inheriting it.
`WorkerConfig::settle_timeout` (`--settle-timeout-ms`, default
`DEFAULT_SETTLE_TIMEOUT`, `0` disables) reaches the embedded factory, because the
in-process path builds `pi_acp::Config::default()`, which hardcodes 600 s and
never reads `PI_ACP_SETTLE_TIMEOUT_SECS`. The catalogue, session-list and history
paths pass `0`: they run no turn.

Measured on this checkout: `cargo check --workspace --locked` clean;
`cargo clippy --workspace --all-targets --locked -- -D warnings` clean;
`cargo test -p loom-worker --lib --locked` 136 passed; `cargo test -p loom-worker
--test acp_session --locked` 13 passed.

## The run budget bounds silence, not the turn (2026-09-22)

A session died with `ACP agent did not settle within 1800000ms` while it was
demonstrably alive. The timeline (`thread_history_row` for
`thr_01M33T9XHH384D1XDEH5ZMB1PV`) showed what happened: the pi agent had called
the **`pi-fork` extension's `fork` tool** — a child `pi` process running a
reconnaissance task — and that one call produced **39858
`item/toolCall/progress` frames**, streaming from 06:42:34 to 07:11:48. The
worker's wall clock ended the run at 1800 s anyway, 16 seconds after the last
frame.

The `v0.5.2` upgrade above had already made the *adapter's* settle budget
activity-aware; the worker's own `run_timeout` had not, so the same class of bug
was still reachable one layer up — and it fired. What changed:

- `ProviderRun::timeout` (`--run-timeout-ms`, default 30 min) now bounds the
  run's **silence**. Every report re-arms it, and a *running call* that started
  without completing holds it off (`RunLiveness` in
  `crates/worker/src/acp/session.rs`), so a thirty-minute tool, build or forked
  child agent cannot trip it. Only work counts
  (`ThreadEventItem::is_running_call`): a message has no completion in the
  contract, so counting one would hold the bound off for the rest of the run.
- `ProviderRun::ceiling` (`--run-ceiling-ms`, default 6 h) is the last-resort
  bound that applies regardless of activity — the only one an agent wedged with
  a tool call open reaches. `0` removes either bound.
- The control plane repeats the same arithmetic in
  `RunRecord::refresh_deadline` / `RunRegistry::observe_event`: its deadline is
  recomputed from the run's last report and its open calls on every report, so
  the server's backstop no longer reaps a run the worker is still nursing.
  `AppConfig::run_ceiling` mirrors the worker's flag and must not be set below
  it.
- A budget expiry says which budget fired — `the agent produced no events for
  1800000ms…` or `the run exceeded its 21600000ms ceiling` — and is categorized
  `budget-exceeded` rather than `connection-failed`, which had claimed the
  connection failed while nothing was wrong with it. (bb's category set has no
  timeout value and `contracts/bb` is exported from bb, so `budget-exceeded` —
  contractually "a budget was exceeded" — is the one that fits without
  diverging.)

Frame handling in the same pass, because the frames *were* there and said
nothing: a v2 tool frame's progress is the frame's **content**, not the call's
restated `title` (reading the title is what turned the fork's stream into 39858
identical `"fork"` rows), and a frame repeating the last line is dropped — on
both the v1 and v2 paths. The fork's real progress (the child's own last
activities, which `pi-fork` streams as content) now reaches the timeline.

`loom server` takes the same two budgets (`--run-timeout-ms`,
`--run-ceiling-ms`) and `--local-worker` passes them to the child it starts, so a
single pair of flags describes both ends of one bound; unset leaves each side's
default in place.

Measured on this checkout: `cargo fmt --all -- --check` clean;
`cargo clippy --workspace --all-targets --locked -- -D warnings` clean;
`cargo test -p loom-worker --lib --locked` **153 passed**;
`cargo test -p loom-server --lib --tests --locked` 385 + integrations passed.
Three tests that drive the **real `pi`** fail on this machine because it has no
model credentials (`pi process exited (code=1)`, "No API key found for the
selected model"): `acp_session::a_real_agent_drives_a_run_to_exactly_one_terminal_event`,
`acp_session::the_thread_is_identified_before_any_update_about_it`, and
`acp_dispatch::an_acp_dispatch_runs_through_a_real_worker`. The failure cannot be
the watchdog: it is a transport error raised during session construction, and
the runs last under a second against a 60-second budget. A probe of the same
path printed `pi process exited (code=1) … pi-acp does not restart pi
automatically`, and `pi --mode json -p …` standalone prints "No API key found
for the selected model" here. CI skips these anyway (no sibling `pi-acp`).

## The agent's command list reaches the composer (2026-09-22)

`/` in the composer reads `projects.commands`, which was answered by a workspace
scan of `pi-acp`'s prompt files plus its built-in list. That missed everything
the agent advertises for itself — package prompt templates, skills — and mangled
the rest: every file command was labelled `origin: project`, and a built-in's
argument hint was dropped.

The scan is now faithful and the advertisement is merged over it:

- A file command's `origin` comes from the `(user)`/`(project)` label `pi-acp`
  puts on the description, a built-in keeps its declared description and
  `argumentHint`, a project prompt shadows a user prompt of the same name, and a
  prompt file shadows a built-in (`command_rows` in
  `crates/worker/src/workspace.rs`).
- `AvailableCommandsUpdate` carries no timeline fact, so the worker keeps it out
  of the event stream and reports it on its own frame
  (`ProviderCommandsReport` → `CommandsReport`) keyed by `(host, provider, cwd)`;
  `crates/server/src/commands.rs` holds the latest list per workspace in memory
  and `b7.rs` merges it **additively** over the scan — the scan's row wins for a
  name it already answered (it is the one with an origin and a hint),
  `skill:<name>` is attributed `source: skill, origin: user`, and anything else
  the agent advertises beyond the scan is its own (`origin: builtin`).
- **`PROTOCOL_VERSION` moves to `4`.** The new frame is a wire change, so an
  older server cannot parse it and the version is what makes an older worker
  upgrade before sending one. The docs that named `3` were updated with it
  (`upgrades.md`, `deployment-verification.md`, `process-model.md`,
  `containers.md`).

Verified on this checkout: `cargo fmt --all -- --check` clean; `cargo clippy
--workspace --all-targets --locked -- -D warnings` clean; the full Rust suite
plus the app's `typecheck` and 502 vitest tests. The new frame's ownership rule
has its own test in `crates/server/tests/ws.rs`, and the merge its own in
`crates/server/tests/b7_conformance.rs`.

## Immediate next steps

1. **Start migration step 1 (ACP adapter boundary)** — unblocked. The
   `SessionUpdate` → event mapping must handle both protocol versions, including
   the `CurrentModeUpdate` asymmetry and v1's lack of a typed `Other`.
2. **Wire interactions up, as a projection/UI task rather than a protocol one.**
   The requests already arrive; loom answers them with `cancelled`
   (`auto_cancel_response` on the Pi path). Replacing that with a real
   `Interaction` is what makes the five B3 routes serve data. Not part of the
   provider migration.
3. **Plan events have no upstream source on the Pi path** — `pi-acp` emits none.
   Either add them there or leave the plan routes refusing; do not invent a
   producer.
4. Then B4, ideally split into two smaller issues given the context-limit
   experience on B3.

Documentation defects are fixed and `d7fbf72` is confirmed non-existent, so
neither blocks anything.
