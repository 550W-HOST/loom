# Handoff

State of the work, what is verified, and what is next. Written for whoever picks
this up — including a future session with no memory of the reasoning.

The current WIP acceptance was measured in this checkout on 2026-09-14 at
`93a4e8f` (`origin/main`) unless a section is explicitly marked historical.
Where something could not be confirmed, it says so rather than guessing.

## Where things stand

| | |
| --- | --- |
| Branch | `agent/openai/77a282b20835`, based on `origin/main` (`93a4e8f`) |
| Working tree | clean at start of W-582; this handoff update is the only product change |
| Rust tests | **696 passing**, 0 failing, **6 ignored** (`cargo test --workspace --locked`) |
| Route coverage | **149 / 149** (100%), pending 0 (`node scripts/check-api-coverage.mjs`) |
| Crates | `relay`, `relay-hub`, `server`, `daemon`, `domain`, `provider-protocol`, `contract` |
| UI | `pnpm install --frozen-lockfile`; build, typecheck and **18 tests** pass |
| Rust quality | `cargo fmt --all -- --check` and clippy `-D warnings` pass |
| Release | `cargo build --workspace --release --locked` passes; release server/daemon started on x86_64 GNU |
| Current WIP decision | **BLOCKED**: core UI environment binding, error projection and permission controls are P1 gaps; W-583, W-584 and W-585 track them |

The six ignored Rust tests are not passes: one ACP provider-e2e test, two real
Pi tests requiring the `pi` CLI and configured model credentials, and three
self-update tests requiring a real daemon binary and fake network endpoint.
The real Pi path remains unverified in this environment.

## W-582 WIP acceptance (2026-09-14)

The release build was exercised with a real server and independently started
daemon processes. The ACP provider in the socket-path run was an explicit JSON-RPC
ACP stub, not Pi; the repository's ACP stub tests and daemon integration tests
were counted separately from the real-Pi ignored tests.

| Scenario | Result | Evidence |
| --- | --- | --- |
| Release server, daemon and UI build | **PASS** | `cargo build --workspace --release --locked`; `pnpm build` |
| API coverage | **PASS** | 149/149 effective routes, pending 0 |
| Rust/UI quality gates | **PASS** | fmt, clippy, 696 Rust tests, 18 UI tests, typecheck |
| Server-only bind and embedded UI | **PASS** | release binary on `127.0.0.1`; `/health`, `/api/v1/version`, `/`, `/app.js` all answered |
| Project/source/environment/thread setup | **PASS via API** | two real daemon identities, two host-bound unmanaged environments and threads |
| ACP first turn, streamed timeline and same-session resume | **PASS with ACP stub** | output deltas, one terminal event, provider session `verify-session` reused after server + daemon restart |
| Permission allow/deny/cancel in UI | **FAIL / P1** | no UI interaction surface; tracked by W-585 |
| Browser project/thread/send flow | **FAIL / P1** | reference UI always creates `project-default` threads with no environment; tracked by W-583 |
| Browser error rendering | **FAIL / P1** | clean browser run shows `Timeline projection failed ... turn/completed without turn/started`; tracked by W-584 |
| Desktop/mobile layout | **PASS for checked shell** | Chromium 1280x720 and 390x844; no horizontal overflow or viewport overlap in captured states |
| Relay replay and domain persistence | **PASS** | 9 thread frames before and after server restart; server rebuilt 27 events from disk log |
| Host identity and host routing | **PASS** | daemon state file reused the same host id; disconnected second host returned `502 host_unavailable` without fallback |
| Filesystem read/write/conflict/containment | **PASS** | real daemon workspace read, optimistic conflict, write and `../` rejection |
| Terminal create/input/output/resize/restart/close | **PASS** | real daemon PTY flow; output cursor returned bounded chunks and `truncated: false` |
| Real Pi provider | **UNVERIFIED** | credentials/CLI unavailable; ignored tests were not counted as pass |
| systemd install/self-update/release publication | **UNVERIFIED** | no production systemd host or published release was available |

The socket-path evidence is intentionally narrower than a real Pi acceptance:
the server, relay, daemon, ACP translation, persistence and host boundaries are
production code, while the provider executable was a transparent ACP stub. The
backend permission tests pass, but that does not substitute for a browser UI.

## Historical: an unverified commit reference

The previous handoff mentioned `d7fbf72` as a documentation commit that needed
an amendment. **It does not exist.** The reference remains historical, not a
current baseline. It was checked in the earlier checkout and against the
remotes:

- `bb`: not in the object database, no reflog entry, no dangling object, not in
  any local or remote branch, not among the `refs/pull/*` heads
- `pi-acp`: not present
- The GitHub API returns 404 for `550W-HOST/loom` and 422 ("No commit found for
  SHA") for `AndPuQing/pi-acp`
- `git fetch origin d7fbf72` fails on both: "couldn't find remote ref"
- Every git repository under `/data/workspace/ljm/dev/`
- Every multica workspace checkout under `/home/ljm/multica_workspaces/`

The current `origin/main` baseline for this handoff is `93a4e8f` (W-571,
2026-09-14). The old `f3a1c15` reference belonged to the prior handoff and is
not the revision accepted here.

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
real daemon, and calling all fourteen routes by hand. Results:

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

### Interaction producer: backend bridge done, browser UI remains

The earlier claim that loom could not produce an interaction is stale. W-566
added the real ACP permission bridge: the daemon sends an `InteractionRequest`,
the server validates its host, run and thread ownership, persists and de-duplicates
the pending `Interaction`, and publishes the resolution back to the requesting
host. Pending requests are cancelled when their run ends, and the bridge has
server, daemon and persistence coverage.

The remaining acceptance gap is entirely on the browser side. The current UI does
not render pending interactions or provide allow, deny and cancel controls, so the
backend permission path is covered but the browser permission scenario is still
**FAIL / P1** (W-585). The capability-gated `session/list` probe may cancel a
request because it has no thread that can answer it; that is separate from a run's
permission request and is intentional.

The neighbouring event gaps are narrower:

- `ProviderEvent::ThreadGoalUpdated` — **no producer at all.** A goal is a
  projection of the run log (`crates/server/src/http.rs:3706` explains the
  design), and nothing ever writes a goal into that log. Its counterpart,
  `ThreadGoalCleared`, *does* have exactly one producer: the `goal/clear` route
  itself publishes it. So `goal/clear` clears a projection that nothing sets —
  idempotent and honest, but not yet useful.
- `ProviderEvent::TurnPlanUpdated` is now mapped from ACP v2 `PlanUpdate`.
  `Plan`, `PlanSteps` and `ItemPlanDelta` still have no provider producer, and
  the default Pi/v1 path does not emit plan updates. The plan routes therefore
  remain explicit about unsupported operations rather than inventing events.

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
| B8 | Host and environment connectivity | 14 | done (W-570) |
| B9 | Files and terminals | 17 | done (W-572) |
| B10 | Settings and system preferences | 13 | done (W-571) |

B5 covers thread counts, pane actions, and host file / thread storage reads. It
depends on B1 and B4, both of which are done. Its one protocol addition is
`HostFileRequest` / `HostFileReport`: a file read is a request to the thread's
host, published through the relay, so the control plane never reads its own disk
and calls the result a thread's file. See `docs/contract.md` ("B5") for the
permission scopes and the two-half traversal defence.

B7 applies the same rule to a project: its workspace files are read from the
project's source host, and an upload is a `HostFileOperation::Write` the daemon
confines to the host's own `project-attachments/<project_id>` directory. It also
made `Project` orderable (`sort_key`), `Project` deletable (a tombstone) and
`ThreadSection` a real entity; see `docs/contract.md` ("B7").

The batch history also records why later work should stay scoped: B1/B2/B3 all
touched `crates/server/src/http.rs`, and W-557 once exceeded the 200k context
limit. That is historical planning context now; B1-B10 are complete and the
coverage checker reports the current 149 / 149 effective routes. `W-541`, the
parent tracker, is **done**.

## The ACP migration: state

`docs/provider-strategy.md` is the decision record. The decisions:

| Question | Decision |
| --- | --- |
| One protocol or several? | **ACP only.** Pi is not special-cased at the client. |
| How is Pi reached? | **`pi-acp` embedded as a library**, over `Channel::duplex()`. |
| ACP version | **v2 first, v1 fallback.** The SDK connector negotiates the highest protocol the agent accepts; v1 remains required for stable agents and the default Pi path. |
| Resume entry point | The next run carries the stored provider session binding and uses `session/resume` under v2 or `session/load` under v1. |
| Unsupported capability | **Reported, never worked around.** |
| Unmapped update type | **Stored and logged, not rendered.** v2 uses `SessionUpdate::Other`; v1 intercepts the raw JSON-RPC frame because its typed schema has no catch-all. No synthetic event is emitted. |

W-564 delivered the stateful event translator, W-566 delivered the permission
bridge, session import boundary and recovery checks, and W-567 delivered v2
negotiation and multi-agent capability handling. loom now depends on `pi-acp`
and the ACP SDK. `ProviderLaunch` has only two ACP forms: `AcpEmbeddedPi` for Pi
and `AcpStdio` for native agents. The old `effective_argv`/`--session-dir`/
`--session-id` path and direct Pi JSON-RPC mapper have been removed.

The current ACP negotiation and resume flow is:

```text
first run:  initialize → session/new → returned sessionId → thread/identity → persist id + agent/cwd binding
next run:   dispatch binding → session/resume (v2) or session/load (v1) → suppress history replay → prompt
permission: session/request_permission → durable Interaction → allow/deny/cancel → resolution frame to host
import:    capability-gated session/list; Unsupported is omitted, not treated as an empty list
```

The server stores the opaque id and its agent/cwd binding with the thread and
includes it in the next `RunDispatch` only when the binding still matches. loom
never reads an agent session file. The adapter serializes construction/report
ordering, checks the negotiated restore capability before resuming, and treats a
missing workspace or unsupported restore as an explicit run failure. A real
second-run regression test is in `crates/daemon/tests/acp_session.rs`.

ACP v2 remains an unstable draft in the pinned SDK, but negotiation and its
adapter mapping are enabled. v1 remains the stable compatibility path; the
acceptance matrix above deliberately used an explicit ACP stub and therefore
does not claim that a real Pi run passed.

Open questions carried forward:

- Does the embedded `pi-acp` need process isolation? A panic in the translator
  would take the daemon with it; a spawned process would not.
- How long to keep the v1 path? v2 is still a draft and may rename things again;
  deprecate v1 only when stable agents stop speaking it.

## Environment notes

- `LOOM_BIND` is the env var for the server's listen address (not
  `LOOM_LISTEN`); default `127.0.0.1:38886`. Others: `LOOM_DATA_DIR`,
  `LOOM_NODE_ID`, `LOOM_REDIS_URL`, `LOOM_UI_DIR`, `LOOM_ARTIFACT_DIR`
- daemon: `LOOM_SERVER_URL`, `LOOM_HOST_NAME`, `LOOM_DAEMON_STATE`,
  `LOOM_AUTO_UPDATE`, and others listed at the top of `crates/daemon/src/main.rs`
- A stale `target/debug/loom-server` from an earlier session held the default
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

## Immediate next steps

1. **Resolve the three P1 browser gaps**: bind UI-created threads to a runnable
   environment (W-583), project pre-run and terminal errors honestly (W-584),
   and render permission interactions with allow/deny/cancel controls (W-585).
2. **Repeat the W-582 acceptance after those fixes**, including the browser
   send/stream/error/permission flows and both checked viewports. W-582 stays
   blocked while any of these core paths is open.
3. **Run the remaining environment-dependent checks** when available: a real Pi
   first turn and resume, systemd installation, self-update and release
   publication. Ignored tests and unavailable infrastructure remain
   **UNVERIFIED**, not passes.

The route batches and ACP migration are complete. Documentation defects are
fixed and `d7fbf72` is confirmed non-existent, so neither blocks anything.
