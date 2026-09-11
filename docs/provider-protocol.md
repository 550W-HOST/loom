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
                   "args": ["--mode", "rpc", "--no-session"] },
  "deadline_ms": 1789120438372,
  "created_at_ms": 1789120430000
}
```

```jsonc
// ProviderReport — daemon -> server socket
{
  "host_id":   "host_01M…",
  "run_id":    "run_01M…",
  "thread_id": "thr_01M…",
  "event":     { "type": "output", "stream": "assistant", "text": "hello " }
}
```

`ProviderReport.event` is a `RunEvent`:

| `type` | meaning |
| --- | --- |
| `started` | the provider process is up |
| `output` | a chunk of `assistant` / `thinking` / `log` text |
| `tool_call` | a tool call began (`tool_call_id`, `name`, `args`) |
| `tool_result` | a tool call finished (`ok`, `output`) |
| `turn` | a turn boundary (`started` / `began` / `ended`) |
| `notice` | anything else the provider reported |
| `finished` | **terminal**: `completed` / `failed` / `timed_out` / `host_stale` / `cancelled` |

On the wire, the server stores each of these as a domain event on the thread
scope:

```jsonc
{ "type": "thread_run_event", "thread_id": "thr_…", "project_id": "proj_…",
  "run_id": "run_…", "at_ms": 1789120438372,
  "event": { "type": "output", "stream": "assistant", "text": "hello " } }
```

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
cargo run -p loom-daemon -- --server-url http://127.0.0.1:38886 --name laptop

# Post a message; the run is dispatched over the relay and its events land in
# the thread scope, replayable at /api/v1/replay.
curl -X POST localhost:38886/api/v1/threads -H 'content-type: application/json' -d '{}'
curl -X POST localhost:38886/api/v1/threads/<thread>/messages \
  -H 'content-type: application/json' -d '{"content":"hello"}'
curl 'localhost:38886/api/v1/runs'
```

An operator can override the provider executable on a machine with
`LOOM_PROVIDER_CMD` / `LOOM_PROVIDER_ARGS`, and cap a run with
`LOOM_RUN_TIMEOUT_MS`.

## Test coverage

| behaviour | where |
| --- | --- |
| stdout guard, Pi frame mapping, one terminal per process | `crates/daemon/src/provider.rs` unit tests |
| dispatch, report, host ownership, timeout + stale reaping | `crates/server/src/runs.rs` unit tests |
| normal turn, provider crash, provider timeout, missed-dispatch replay, silent-daemon reaping | `crates/daemon/tests/provider_e2e.rs` (real sockets, real processes) |
| the real `pi` binary | same file, `#[ignore]`d |
