# The provider execution contract

This document is the boundary between `loom-server` (Rust control plane) and a
daemon's provider execution (`loom-daemon` here; bb's Node `apps/host-daemon`
later). It is the provider-shaped companion to [`process-model.md`](process-model.md):
that document answers *how server and daemon relate*; this one answers *how a
thread turn crosses the boundary*.

## Two directions, one rule each

```text
                     relay  host:{id}                     thread:{id}
  server ── RunDispatch ─────────▶ daemon ── ProviderReport ──▶ server ──▶ relay
             (published to a scope)          (up the daemon's own socket)     │
                                                                              ▼
                                                             replayable client frames
```

* **Downward: dispatch goes through the relay.** The control plane mints a
  `run_id`, records the run, and publishes a `RunDispatch` to `host:{host_id}`.
  It never looks at a daemon socket. Consequences: a daemon that is reconnecting
  still gets the run, and dispatch is replayable.
* **Upward: reports go up the daemon's own socket.** A report is an
  observation, not a command. The server turns it into a `thread_run_event` and
  publishes it to `thread:{thread_id}` **through the relay**, so provider
  output, tool calls and turn lifecycle are replayable like any other event and
  the daemon socket is never a fan-out path.

## The wire types

They live in `crates/provider-protocol` (`loom-provider-protocol`): plain data,
no runtime, no relay dependency.

```jsonc
// RunDispatch — server -> relay host:{id} -> daemon
{
  "run_id":      "run_01M…",
  "thread_id":   "thr_01M…",
  "project_id":  "proj_01M…",
  "host_id":     "host_01M…",
  "prompt":      "fix the failing test",
  "provider":    { "name": "pi", "command": "pi",
                   "args": ["--mode", "rpc", "--no-session"],
                   "cwd": "/srv/projects/loom" },
  "deadline_ms": 1789120438372,
  "created_at_ms": 1789120430000
}
```

`provider.cwd` is the thread's **environment workspace**, and it is the whole
reason a dispatch carries a provider spec rather than a bare command: a
provider that starts in the daemon's own cwd does not know what project it is
editing. See [the workspace section](#the-workspace-is-part-of-the-dispatch).

```jsonc
// ProviderReport — daemon -> server socket
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
      "providerThreadId": "thr_01M…",
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

The events the daemon produces from Pi frames:

| contract `type` | Pi frame | note |
| --- | --- | --- |
| `thread/identity` | `agent_start` (first) | `providerThreadId` is the thread id |
| `turn/started` | `agent_start` | |
| `item/agentMessage/delta` | `message_update` / `text_delta` | assistant answer channel |
| `item/reasoning/textDelta` | `message_update` / `thinking_delta` | reasoning channel, a distinct type |
| `item/started` | `tool_execution_start`, `compaction_start` | `bash` → `commandExecution`, `edit`/`write` → `fileChange`, `read` → `fileRead`, `grep`/`find`/`ls` → `search`, else `toolCall` |
| `item/completed` | `tool_execution_end`, `text_end`, `thinking_end`, `compaction_end` | |
| `item/commandExecution/outputDelta` | `tool_execution_update` (bash) | `reset: true`, because Pi sends a snapshot |
| `item/toolCall/progress` | `tool_execution_update` (other) | |
| `thread/compacted` | `compaction_end` (success) | |
| `thread/tokenUsage/updated` | `agent_end` | from the assistant `usage` block |
| `provider/error` | `agent_end` (`stopReason: error`), retry failure | |
| `provider/warning` | `extension_error`, declined dialog | |
| _(unmapped)_ | anything else | reported on stderr; **no event**. loom has no `provider/unhandled` fallback body (see [`event-model.md`](event-model.md)) |
| `turn/completed` | `agent_settled`, rejected prompt, exit, timeout | **terminal** |

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

## The workspace is part of the dispatch

A thread's execution context is an **environment** (`env_…`): a directory on a
host, either *unmanaged* (it already exists; loom never removes it) or *managed*
(loom creates it). The server owns the registry and the lifecycle; the daemon
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
2. **The daemon validates the directory, never falls back.** Before spawning,
   the daemon checks `provider.cwd` is a directory *on its own machine*. A
   missing one fails the run with a reason naming the path; it is not a license
   to use the daemon's cwd. See `provider::drive`.
3. **An operator override replaces the executable, not the workspace.**
   `LOOM_PROVIDER_CMD` swaps what runs; `cwd` still comes from the dispatch.

A thread with **no environment**, or one whose environment is not `ready`, is
failed on dispatch with an explicit error. There is deliberately no default
directory: a silent default is the bug this contract fixes.

### Managed environments: provisioning

An unmanaged environment is `ready` the moment it is created. A managed one
starts `creating` and is provisioned by a daemon:

```text
  creating ──▶ provisioning ──▶ ready
                    │
                    └─ failure ──▶ error ──(retry)──▶ provisioning
```

The request travels **through the relay** like a dispatch, and the outcome
comes back up the daemon's socket like a report:

```jsonc
// EnvironmentProvision — server -> relay host:{id} -> daemon
{ "environment_id": "env_01M…", "project_id": "proj_01M…",
  "host_id": "host_01M…", "created_at_ms": 1789120430000 }

// EnvironmentProvisionReport — daemon -> server socket
{ "host_id": "host_01M…", "environment_id": "env_01M…",
  "outcome": { "outcome": "provisioned", "path": "/root/env_01M…" } }
// or
{ "host_id": "host_01M…", "environment_id": "env_01M…",
  "outcome": { "outcome": "failed", "error": "could not create …: permission denied" } }
```

The daemon creates `<workspace_root>/<env_id>` (`LOOM_WORKSPACE_ROOT`, default
`$HOME/.loom/workspaces`); the control plane only learns the resulting path
from the report, then records it and publishes `environment_status_changed` to
`project:{id}`. A failed attempt moves the environment to `error` with the
daemon's reason attached, and `POST /api/v1/environments/{id}/provision` retries
it. Creation is idempotent (`create_dir_all`), so a redelivered request is
safe.

## The invariant: a run always ends

> Every run reaches exactly one `finished` event, and the thread leaves
> `working` because of it.

This is the class of bug the contract exists to eliminate. bb's
`#1180` / `#1633` / `#3187` are all cases where the provider vanished and the
thread stayed `working` forever. The contract makes that unrepresentable by
giving the terminal state **two independent owners**:

1. **The daemon** guarantees it per process. `loom-daemon`'s bridge maps a
   terminal Pi frame, a non-zero exit, an exit with no settle, or a deadline to
   exactly one `finished`. There is no code path that returns without one.
2. **The server** guarantees it per run. `AppState::reconcile_runs` reaps a run
   whose deadline passed (`timed_out`) and every run on a host that stopped
   heartbeating (`host_stale`). It does not trust the execution plane to report
   its own death, because that is exactly what a dead execution plane cannot do.

The thread transition is idempotent: a terminal event for a run already reaped
is dropped, and a second status change is not produced.

## The stdout guard

A provider is not required to keep stdout clean, and bb #1180 is what happens
when a bridge assumes it is: Pi emitted an OSC 777 desktop notification on
stdout, the bridge fed it to a JSON parser, and the thread wedged.

Every stdout line therefore passes `guard_stdout_line` *before* the frame
mapper:

* a JSON **object** with a **string `type`** is a frame;
* everything else — log lines, OSC escapes, JSON scalars or arrays, objects
  without `type` — is routed to stderr and never reaches the mapper.

Framing is also strict JSONL: split on `LF` only, strip a trailing `CR`. Pi's
RPC docs call this out explicitly because Node's `readline` also splits on
`U+2028` / `U+2029`, which are legal inside JSON strings.

## Idempotence and reconnect

* `run_id` is the idempotency key. A daemon keeps a bounded set of run ids it
  has started, so a redelivered dispatch is dropped rather than run twice.
* A daemon persists its host-scope event cursor and, on reconnect, subscribes
  **then** replays from that cursor (`ClientCommand::Replay`). Live frames that
  arrive in between are also in the replay window; the event-id dedup set drops
  the overlap. This is what makes "dispatch published while the daemon was
  away" arrive late instead of being lost.

## What is deliberately not here

* **No plugin system.** A provider is a `ProviderSpec` — a command plus
  arguments. There is no manifest, no discovery, no marketplace, no lifecycle
  hooks.
* **No provider protocol in the control plane.** The server never parses Pi's
  event shape. The daemon normalises it. That is what keeps a provider's stdout
  format from coupling into the control plane.
* **No FFI, no embedded runtime.** The boundary is a network boundary.

## Running it

```bash
# Server-only, as usual.
cargo run -p loom-server

# A daemon that runs whatever provider the control plane dispatches (pi).
# LOOM_WORKSPACE_ROOT is where it will create managed environments' workspaces.
cargo run -p loom-daemon -- --server-url http://127.0.0.1:38886 --name laptop

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
`LOOM_PROVIDER_CMD` / `LOOM_PROVIDER_ARGS`, cap a run with
`LOOM_RUN_TIMEOUT_MS`, and choose the managed-workspace root with
`LOOM_WORKSPACE_ROOT`. The override never changes the workspace.

## Test coverage

| behaviour | where |
| --- | --- |
| stdout guard, Pi frame mapping, one terminal per process | `crates/daemon/src/provider.rs` unit tests |
| dispatch, report, host ownership, timeout + stale reaping, `cwd` from the environment | `crates/server/src/runs.rs` unit tests |
| environment registry, lifecycle, provisioning dispatch + reports | `crates/server/src/domain_state.rs`, `crates/server/src/environments.rs` unit tests |
| environment HTTP API (create/list/get/destroy, path validation) | `crates/server/src/http.rs` unit tests |
| normal turn, provider crash, provider timeout, missed-dispatch replay, silent-daemon reaping | `crates/daemon/tests/provider_e2e.rs` (real sockets, real processes) |
| provider runs *in* the bound workspace; a missing workspace fails clearly; managed provisioning succeeds and fails | same file |
| the real `pi` binary | same file, `#[ignore]`d |
