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
| Tests | **461 passing**, 0 failing (`cargo test --workspace --locked`) |
| Route coverage | **53 / 149** (35.6%) — `docs/api-coverage.md` |
| Crates | `relay`, `relay-hub`, `server`, `daemon`, `domain`, `provider-protocol`, `contract` |
| UI | `ui/` workspace, 18 tests, typecheck clean |
| CI | fmt + clippy `-D warnings` + test + MSRV + contract reproducibility + UI + pi |
| Test count history | 253 → 416 (B2) → **461** (B3, +45) |

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

## Real documentation defects, both now fixed

These were found and fixed in the session that wrote this document. Kept on the
record because the *pattern* is worth watching: a decision was made in one
document and the document it superseded was not updated.

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

## pi-acp: done, and verified here

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
| `GET /threads/:id/interactions/:id` | 404 (no producer — see below) |
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

### The honest gap: interactions have no producer

`record_interaction` and `create_interaction` have **no callers outside tests**.
Verified:

```
$ grep -rn "record_interaction\|create_interaction" crates/ --include=*.rs | grep -v "/tests/"
crates/server/src/interactions.rs:63:    pub fn record_interaction(
crates/server/src/interactions.rs:75:        let (interaction, event) = self.registry.create_interaction(
crates/server/src/domain_state.rs:914:    pub fn create_interaction(
```

And the provider protocol cannot express one either — `provider-protocol` has
`ProviderSpec`, `RunDispatch`, `EnvironmentProvision`,
`EnvironmentProvisionOutcome`, `EnvironmentProvisionReport`, `ProviderReport`,
`GuardedLine`, and no interaction frame.

So: the five interaction routes are implemented, contract-shaped, persisted, and
tested — but in a running system nothing ever creates an interaction to serve.
The domain and HTTP layers are real; the producer is missing.

Same shape of gap for two neighbouring things, though the details differ:

- `ProviderEvent::ThreadGoalUpdated` — **no producer at all.** A goal is a
  projection of the run log (`crates/server/src/http.rs:3706` explains the
  design), and nothing ever writes a goal into that log. Its counterpart,
  `ThreadGoalCleared`, *does* have exactly one producer: the `goal/clear` route
  itself publishes it. So `goal/clear` clears a projection that nothing sets —
  idempotent and honest, but not yet useful.
- `ProviderEvent::Plan`, `PlanSteps`, `TurnPlanUpdated`, `ItemPlanDelta` — same
  as `ThreadGoalUpdated`: defined, mapped to contract event names, never
  constructed outside tests.

These are honest gaps rather than bugs: the routes refuse or no-op correctly.
But B3's domain concepts are not reachable through the product path yet, and
that should be stated plainly rather than discovered later. Whether the producer
belongs in this batch or in the ACP migration is a decision for the next
session — **with ACP, interactions and plans arrive as ACP `SessionUpdate`s**
(request permission, plan updates), so the producer may naturally land as part
of step 2 of the migration rather than as separate work.

## Route batches

| Batch | Theme | Routes | Status |
| --- | --- | ---: | --- |
| B0 | Baseline | 53 | done |
| B1 | Startup, navigation, first threads flow | 14 | done (W-548) |
| B2 | Thread control and auxiliary views | 14 | done (W-556) |
| B3 | Interactions, plans, queue sending | 14 | done (W-557) |
| B4 | Thread lifecycle and queue management | 14 | next |
| B5 | Thread files and storage helpers | 10 | |
| B6 | Environment lifecycle and repo status | 14 | |
| B7 | Project workspace, attachments, sections | 14 | |
| B8 | Host and environment connectivity | 14 | |
| B9 | Files and terminals | 17 | |
| B10 | Settings and system preferences | 13 | |

B4 covers archive, delete, fork, pin/unread, and queued-message delete / reorder
/ update. It depends on B3, which is now done.

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
| ACP version | **v2.** v1 refused rather than degraded. |
| Resume entry point | **`loom resume <thread>`**, backed by `session/load`. |
| Unsupported capability | **Reported, never worked around.** |
| `SessionUpdate::Other` | **Stored and logged, not rendered**; `_` prefix distinguished from future standard variants. |

loom does **not** depend on `pi-acp` yet — `grep pi-acp Cargo.toml
crates/*/Cargo.toml` is empty. The `agent-client-protocol` crate is not a
dependency either. Migration step 1 has not started.

Current Pi path, which step 3 removes:

```rust
// crates/daemon/src/provider.rs:336
pub fn effective_argv(spec, thread_id, session_dir) -> Vec<String> {
    let mut argv = spec.argv();
    if spec.name == "pi" {
        if let Some(dir) = session_dir {
            argv.retain(|arg| arg != "--no-session");
            argv.push("--session-dir".into());
            argv.push(dir.to_string_lossy().into_owned());
            argv.push("--session-id".into());
            argv.push(thread_id.to_string());   // thread id used as session id
        }
    }
    argv
}
```

Migration order from the strategy doc, restated with what each step now needs:

1. **Define the ACP-adapter boundary** in `loom-domain` / `provider-protocol`.
   Not started. Independent of pi-acp, can begin immediately.
2. **Build loom's ACP client**; wire both an embedded peer (`Channel::duplex()`
   → `pi_acp::agent::AcpAgent::run_with`) and a spawned one (`Stdio::new()`).
   Unblocked — pi-acp's half is done. Adds `pi-acp` and
   `agent-client-protocol` as dependencies.
3. **Remove the Pi-specific path** — `effective_argv` rewriting, the `pi`
   special case in `ProviderSpec`.
4. **Add `loom resume <thread>`** plus the import flow over
   `session/load` / `session/list`. Depends on `(thread) → (agent, session_id,
   cwd)` being in the domain snapshot.
5. **Re-decide the daemon's user model.** W-558 is parked: with loom no longer
   reading session files, its "let the daemon see the user's `~/.pi`"
   justification is gone, and it reduces to resource isolation versus
   convenience.

Open questions carried forward:

- Does every target agent implement `session/list`? It is a capability, so no —
  the import flow must omit rather than guess.
- Version policy for the `agent-client-protocol` crate (distinct axis from the
  protocol version).
- Does the embedded `pi-acp` need process isolation? A panic in the translator
  would take the daemon with it; a spawned process would not.

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

1. Decide the interaction/goal/plan producer: same batch, or part of the ACP
   migration. (The ACP route is likely cheaper, since ACP already models
   permission requests and plan updates.)
2. Start migration step 1 (ACP adapter boundary) — unblocked, and independent of
   anything else here.
3. Then B4, ideally split into two smaller issues given the context-limit
   experience on B3.

Documentation defects are fixed and `d7fbf72` is confirmed non-existent, so
neither blocks anything.
