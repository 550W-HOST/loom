# The provider execution contract

This document is the boundary between `loom-server` (Rust control plane) and a
worker's provider execution (`loom-worker` here; bb's Node `apps/host-daemon`
later). It is the provider-shaped companion to [`process-model.md`](process-model.md):
that document answers *how server and worker relate*; this one answers *how a
thread turn crosses the boundary*.

## Two directions, one rule each

```text
                     relay  host:{id}                     thread:{id}
  server ── RunDispatch ─────────▶ worker ── ProviderReport ──▶ server ──▶ relay
             (RunDispatch; RunSteer joins it)          (up the worker's own socket)     │
                                                                              ▼
                                                             replayable client frames
```

* **Downward: dispatch goes through the relay.** The control plane mints a
  `run_id`, records the run, and publishes a `RunDispatch` to `host:{host_id}`.
  It never looks at a worker socket. Consequences: a worker that is reconnecting
  still gets the run, and dispatch is replayable.
* **Downward: a steer joins a dispatched turn the same way.** `RunSteer` goes to
  the same scope through the same relay, so a steer for a run published while
  the worker was away is delivered late rather than lost. See
  [Joining a turn](#joining-a-turn-the-steer-frame).
* **Upward: reports go up the worker's own socket.** A report is an
  observation, not a command. The server turns it into a `thread_run_event` and
  publishes it to `thread:{thread_id}` **through the relay**, so provider
  output, tool calls and turn lifecycle are replayable like any other event and
  the worker socket is never a fan-out path.

## The wire types

They live in `crates/provider-protocol` (`loom-provider-protocol`): plain data,
no runtime, no relay dependency.

```jsonc
// RunDispatch — server -> relay host:{id} -> worker
{
  "run_id":      "run_01M…",
  "thread_id":   "thr_01M…",
  "project_id":  "proj_01M…",
  "host_id":     "host_01M…",
  "prompt":      "fix the failing test",
  "provider":    { "name": "pi", "launch": "acp_embedded_pi",
                   "command": "pi", "args": [],
                   "cwd": "/srv/projects/loom" },
  "provider_session_id": null,
  "deadline_ms": 1789120438372,
  "created_at_ms": 1789120430000
}
```

`provider.cwd` is the thread's **environment workspace**, and it is the whole
reason a dispatch carries a provider spec rather than a bare command: a
provider that starts in the worker's own cwd does not know what project it is
editing. See [the workspace section](#the-workspace-is-part-of-the-dispatch).

```jsonc
// ProviderReport — worker -> server socket
{
  "host_id": "host_01M…",
  "event": {
    "thread_id": "thr_01M…",
    "project_id": "proj_01M…",
    "run_id": "run_01M…",
    "at_ms": 1789120438372,
    "event": {
      "threadId": "thr_01M…",
      "scope": { "kind": "turn", "turnId": "run_01M…" },
      "providerThreadId": "pi-acp-session-1",
      "type": "item/agentMessage/delta",
      "itemId": "assistant-1",
      "delta": "hello "
    }
  }
}
```

`ProviderReport.event` is a `RunEvent`: loom's envelope (`thread_id`,
`project_id`, `run_id`, `at_ms`) around a **bb `ThreadEvent`**. The inner event
is the contract's own shape — `type` discriminant and camelCase fields — so
bb's projection layer consumes it unchanged. See
[`event-model.md`](event-model.md) for the full type map and the per-type
decisions.

The events the worker produces from ACP updates (Pi uses `pi-acp` internally):

| contract `type` | ACP source | note |
| --- | --- | --- |
| `thread/identity` | `session/new` / `session/load` response | `providerThreadId` is the agent's opaque `sessionId` |
| `turn/started` | before `session/prompt` | ACP has sessions and prompts, not bb turns |
| `item/agentMessage/delta` | `session/update.agent_message_chunk` | assistant answer channel |
| `item/reasoning/textDelta` | `session/update.agent_thought_chunk` | reasoning channel |
| `item/started` | `session/update.tool_call` | ACP tool data is classified when required fields exist |
| `item/completed` | terminal `tool_call_update` / stream flush | |
| `item/toolCall/progress` | non-terminal `tool_call_update` | |
| `turn/plan/updated` | `session/update.plan` | whole plan update |
| `thread/contextWindowUsage/updated` | `session/update.usage_update` | context occupancy, not token counts |
| `thread/name/updated` | `session/update.session_info_update` | only a concrete title is emitted |
| `provider/error` | rejected prompt, transport failure | |
| `turn/completed` | v1 prompt `stopReason` or worker timeout | **terminal** |

On the wire, the server stores the whole envelope as a domain event on the
thread scope:

```jsonc
{ "type": "thread_run_event",
  "thread_id": "thr_…", "project_id": "proj_…",
  "run_id": "run_…", "at_ms": 1789120438372, "outcome": "completed",
  "event": {
    "threadId": "thr_…",
    "scope": { "kind": "turn", "turnId": "run_…" },
    "providerThreadId": "thr_…",
    "type": "turn/completed",
    "status": "completed"
  }
}
```

`outcome` is loom's own verdict and is present only on the terminal event; it
is outside the contract event so the inner payload still validates against
`contracts/bb/thread-event.json`. It is how a deadline (`timed_out`), a stale
host (`host_stale`) and a cancellation (`cancelled`) stay distinguishable even
though the contract folds them into `status`

## Side reports: models and commands

Two facts a session reports are not run events and wait for no run:

| Frame | Scope | Why it is not an event |
| --- | --- | --- |
| `CatalogReport` | `(host, provider)` | the agent's model list belongs to the machine, not to a conversation |
| `CommandsReport` | `(host, provider, cwd)` | the session's command menu is an affordance, and prompt files make it workspace-specific |

Both travel from the worker's socket loop as their own frame, are validated
against the host the connection enrolled as, and are kept in memory on the
server (`catalogs.rs`, `commands.rs`). `projects.commands` serves the command
list as an additive overlay over the embedded pi adapter's workspace scan, and
as the whole menu for any other agent, which has no scan; see
[`contract.md`](contract.md).

## Joining a turn: the steer frame

A user can type while the agent is working, and that input belongs to the turn
already running rather than to a new one. `RunSteer` carries it, and it travels
**downward through the relay** exactly like a dispatch:

```jsonc
// RunSteer — server -> relay host:{id} -> worker
{
  "run_id":       "run_01M…",   // the turn to join; also the idempotency key
  "thread_id":    "thr_01M…",
  "project_id":   "proj_01M…",
  "host_id":      "host_01M…",
  "text":         "actually, use the other fixture",
  "created_at_ms": 1789120430000
}
```

ACP has no "inject into the running prompt" method. Every ACP client therefore
delivers a steer the same way: **cancel the prompt in flight and send the text as
the next prompt on the same session.** Zed calls the control "send immediately"
and does exactly that; bb's provider bridge calls the capability
`steerMode: "queue"` and does exactly that. loom's worker does it too, and keeps
the run open across it, so the timeline reads as **one turn** with the input
appended rather than two. The run's one terminal event still comes from the last
prompt the worker sent, never from the cancelled one — the invariant at the top
of this document is not weakened.

Three consequences worth being explicit about:

1. **The session is preserved.** The steer re-prompts the same ACP session, so
   the model keeps its context and sees the new input at the next tool boundary
   — not after a `session/load` of a fresh process.
2. **A steer can lose a race with the run's end.** A frame naming a run the
   worker is not executing is dropped. If the control plane noticed first (no
   run in flight) it sends the message as a fresh turn instead — bb's
   `appliedAs: "new-turn"` — and if the worker noticed first, the message is
   already recorded on the thread and the next turn sees it. Either way nothing
   is lost.
3. **No second concurrent turn starts.** The server does not mint a run for a
   steer; it publishes the frame to the host that owns the running one.

## The workspace is part of the dispatch

A thread's execution context is an **environment** (`env_…`): a directory on a
host, either *unmanaged* (it already exists; loom never removes it) or *managed*
(loom creates it). The server owns the registry and the lifecycle; the worker
owns the directory layout for managed ones.

```text
  thread.environment_id ──▶ environment.path ──▶ RunDispatch.provider.cwd
                           environment.host_id ──▶ host:{id} target
```

Three rules, all enforced in code:

1. **A dispatch always names its workspace.** The server fills
   `provider.cwd` from the environment's path and targets the environment's
   host — the directory only exists there. See
   `AppState::dispatch_thread` in `crates/server/src/runs.rs`.
2. **The worker validates the directory, never falls back.** Before starting the
   ACP agent, it checks `provider.cwd` is a directory *on its own machine*. A
   missing one fails the run with a reason naming the path; it is not a license
   to use the worker's cwd. See `acp::session::drive`.
3. **An operator override replaces the executable, not the workspace.**
   `--provider-cmd` swaps what runs; `cwd` still comes from the dispatch.

A thread with **no environment**, or one whose environment is not `ready`, is
failed on dispatch with an explicit error. There is deliberately no default
directory: a silent default is the bug this contract fixes.

### Managed environments: provisioning

An unmanaged environment is `ready` the moment it is created. A managed one
starts `creating` and is provisioned by a worker:

```text
  creating ──▶ provisioning ──▶ ready
                    │
                    └─ failure ──▶ error ──(retry)──▶ provisioning
```

The request travels **through the relay** like a dispatch, and the outcome
comes back up the worker's socket like a report:

```jsonc
// EnvironmentProvision — server -> relay host:{id} -> worker
{ "environment_id": "env_01M…", "project_id": "proj_01M…",
  "host_id": "host_01M…", "created_at_ms": 1789120430000 }
// A git-worktree environment carries the selection instead:
{ "environment_id": "env_01M…", "project_id": "proj_01M…",
  "host_id": "host_01M…", "created_at_ms": 1789120430000,
  "workspace": { "kind": "git_worktree", "source_path": "/work/project",
                 "branch_name": "loom/env_01M…", "base_branch": "origin/main" } }

// EnvironmentProvisionReport — worker -> server socket
{ "host_id": "host_01M…", "environment_id": "env_01M…",
  "outcome": { "outcome": "provisioned", "path": "/root/env_01M…",
               "branch_name": "loom/env_01M…", "base_branch": "origin/main",
               "default_branch": "main", "is_git_repo": true } }
// or
{ "host_id": "host_01M…", "environment_id": "env_01M…",
  "outcome": { "outcome": "failed", "error": "could not create …: permission denied" } }
```

With no `workspace` the worker creates `<workspace_root>/<env_id>`
(`--workspace-root`, default `$HOME/.loom/workspaces`) as an empty directory
— the personal-workspace provider. With a `git_worktree` selection it cuts a
worktree there from the source checkout on the same branch, copies the files
`.worktreeinclude` selects, and reports the branch and git facts. The control
plane only learns the resulting path from the report, then records it,
publishes `environment_status_changed` and a whole-record
`environment_updated` (so the path and branch survive replay), and the
environment reaches `ready`. A failed attempt moves the environment to `error`
with the worker's reason attached, and `POST /api/v1/environments/{id}/provision`
retries it. Directory creation is idempotent; a worktree request is re-adopted
when the target already sits on the expected branch (see
[`worktrees.md`](worktrees.md)).

### Managed environments: teardown

`DELETE /api/v1/environments/{id}` moves a managed environment to `destroyed`
with a `running` teardown record and publishes an `EnvironmentDeprovision` to
the host, which removes the worktree or managed directory:

```jsonc
// EnvironmentDeprovision — server -> relay host:{id} -> worker
{ "environment_id": "env_01M…", "project_id": "proj_01M…",
  "host_id": "host_01M…", "path": "/root/env_01M…", "created_at_ms": 1789120430000 }

// EnvironmentDeprovisionReport — worker -> server socket
{ "host_id": "host_01M…", "environment_id": "env_01M…",
  "outcome": { "outcome": "removed" } }
// or
{ "host_id": "host_01M…", "environment_id": "env_01M…",
  "outcome": { "outcome": "failed", "error": "…" } }
```

The report settles `environment.teardown` to `removed` or `failed`; a failure
keeps the reason on the record and `DELETE` again retries. An unmanaged
environment is only moved to `destroyed`: loom never removes a directory the
operator owns. `path` is required so a provisioning request cannot decode as a
teardown.

## The invariant: a run always ends

> Every run reaches exactly one `finished` event, and the thread leaves
> `working` because of it.

This is the class of bug the contract exists to eliminate. bb's
`#1180` / `#1633` / `#3187` are all cases where the provider vanished and the
thread stayed `working` forever. The contract makes that unrepresentable by
giving the terminal state **two independent owners**:

1. **The worker** guarantees it per ACP connection. The ACP driver maps a
   terminal prompt result, a transport exit or a budget expiry to exactly one
   `turn/completed`. The completion signal is taken from **whichever arrives
   first**: v1 reports `stop_reason` on the `session/prompt` response, v2 reports
   it in a `state_update: idle` notification (there is no stop reason on a v2
   response), and the response closes the turn when the notification never
   comes. That last case is not hypothetical — pi-acp drops the idle update when
   its outbound connector dies on a single unconvertible update, which is how a
   real turn used to sit until the run timeout (W-623). The worker's own budget
   is on the run's **silence** and is held off by an item that started without
   completing, so expiring it means the agent stopped saying anything, never
   that it was merely slow; see `docs/acp-adapter.md`.
2. **The server** guarantees it per run. `AppState::reconcile_runs` reaps a run
   whose deadline passed (`timed_out`) and every run on a host that stopped
   heartbeating (`host_stale`). It does not trust the execution plane to report
   its own death, because that is exactly what a dead execution plane cannot do.
   Its deadline is recomputed on every report the run makes, so it too measures
   silence rather than the length of the turn.

The thread transition is idempotent: a terminal event for a run already reaped
is dropped, and a second status change is not produced.

## ACP framing

ACP owns JSON-RPC framing. The worker consumes typed ACP requests, responses and
`session/update` notifications; it does not parse Pi's private JSONL protocol.
Only the embedded `pi-acp` library speaks that private protocol internally.

## Idempotence and reconnect

* `run_id` is the idempotency key. A worker keeps a bounded set of run ids it
  has started, so a redelivered dispatch is dropped rather than run twice.
* A worker persists its host-scope event cursor and, on reconnect, subscribes
  **then** replays from that cursor (`ClientCommand::Replay`). Live frames that
  arrive in between are also in the replay window; the event-id dedup set drops
  the overlap. This is what makes "dispatch published while the worker was
  away" arrive late instead of being lost.

## What is deliberately not here

* **No plugin system.** A provider is a `ProviderSpec` — a command plus
  arguments. There is no manifest, no discovery, no marketplace, no lifecycle
  hooks.
* **No provider protocol in the control plane.** The server never parses ACP
  frames. The worker owns the ACP client, translates `session/update`, and
  reports only the canonical run events.
* **No provider-owned session files in loom.** The ACP agent owns storage;
  loom persists only the opaque `provider_session_id` needed for the next
  `session/load`.

## Running it

```bash
# Server-only, as usual.
cargo run -p loom -- server

# A worker that runs the provider the control plane dispatched. The built-in Pi
# provider is ACP through embedded `pi-acp`; a custom command must be an ACP
# agent speaking stdio.
cargo run -p loom -- worker --server-url http://127.0.0.1:38886 --name laptop

# Create a workspace pointing at an existing project directory, bind a thread
# to it, then post a message. The provider runs in that directory. Both the
# environment and the thread name the project they belong to.
curl localhost:38886/api/v1/projects     # pick a project_id
curl -X POST localhost:38886/api/v1/environments -H 'content-type: application/json' \
  -d '{"kind":"unmanaged","path":"/srv/projects/loom","project_id":"proj_…"}'
curl -X POST localhost:38886/api/v1/threads -H 'content-type: application/json' \
  -d '{"project_id":"proj_…","environment_id":"env_…"}'
curl -X POST localhost:38886/api/v1/threads/<thread>/messages \
  -H 'content-type: application/json' -d '{"content":"hello"}'
curl 'localhost:38886/api/v1/runs'
curl 'localhost:38886/api/v1/environments'
```

An operator can override the provider executable on a machine with
`--provider-cmd` / `--provider-args`, cap a run's *silence* with
`--run-timeout-ms` and its total length with `--run-ceiling-ms`, and choose the
managed-workspace root with `--workspace-root`. The override never changes the
workspace. `loom server` takes the same two run budgets — it is the side that
reaps a run whose worker went quiet — and `--local-worker` hands them to the
child it starts, so one pair of flags describes both ends.

When a run behaves oddly — text arrives but the turn never closes, or an
expected frame is missing — `LOOM_ACP_TRACE=1` makes the worker print what the
ACP boundary received (every notification by name, every translated event, and
every terminal decision). It is the difference between "the agent never said the
turn was over" and "the worker never heard it".

## Test coverage

| behaviour | where |
| --- | --- |
| ACP mapping, session identity, one terminal per run | `crates/worker/src/acp/` unit and integration tests |
| dispatch, report, host ownership, timeout + stale reaping, `cwd` from the environment | `crates/server/src/runs.rs` unit tests |
| environment registry, lifecycle, provisioning dispatch + reports | `crates/server/src/domain_state.rs`, `crates/server/src/environments.rs` unit tests |
| environment HTTP API (create/list/get/destroy, path validation) | `crates/server/src/http.rs` unit tests |
| normal ACP turn, provider crash, provider timeout, missed-dispatch replay, silent-worker reaping | `crates/worker/tests/provider_e2e.rs` (real sockets, real ACP agents) |
| steer routing: the HTTP routes publish `RunSteer`, a steered queued row leaves the queue | `crates/server/tests/b3_conformance.rs` |
| the live-run registry that carries a steer to its conversation (delivery, unknown run, forget) | `crates/worker/src/steer.rs` unit tests |
| a steer cancels the prompt in flight, re-prompts the same session, and the run ends once | `crates/worker/src/acp/session.rs`, `a_steer_cancels_the_prompt_in_flight...` |
| two-run ACP session/load resume and identity persistence | `crates/worker/tests/acp_session.rs`, `crates/worker/tests/acp_dispatch.rs` |
| embedded Pi through `pi-acp` | `crates/worker/tests/acp_embedded.rs` |
| the real `pi` binary | same provider e2e file, `#[ignore]`d |
