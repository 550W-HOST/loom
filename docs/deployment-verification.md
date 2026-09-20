# Deployment verification

One recorded clean-machine run of the supported process path: start the server,
join a worker, dispatch a task, and confirm the run's frames replay. It exists so
the deployment documents are not claims — every command below is a command an
operator runs.

The packaging this document used to exercise — the systemd units, the environment
templates and the install script — is gone. There is no installer and no
environment file: a deployment is the flags on an `ExecStart` line under whatever
supervisor the operator already runs ([`process-model.md`](process-model.md)
§ Deploying it), or the compose file at
[`../containers/docker-compose.yml`](../containers/docker-compose.yml)
([`containers.md`](containers.md)). What is still worth verifying is the runtime
path, and that is what this run covers.

The goal is the acceptance path, not a benchmark: server-only startup, a
worker-only process on the same host joining over loopback, the UI served from
the API's origin, a thread turn dispatched through the relay, and the run's
events replayable from the retained window. A second run covers the restart
claims in [`upgrades.md`](upgrades.md).

## Environment

| | |
| --- | --- |
| Host | Ubuntu 20.04.6 LTS, x86_64, kernel 5.15 (clean workspace) |
| Revision | `a0f8730` + this change set |
| Toolchain | `rustc 1.98.0`, build profile `release` (`cargo build --release -p loom`) |
| Binary | `target/release/loom`, run once as `loom server` and once as `loom worker` (the recorded run below predates the one-binary change and used `target/release/loom-server` and `target/release/loom-worker`) |
| Server flags | `--bind 127.0.0.1:38899` (a test port; the default is `38886`), `--data-dir` for the durable log, `--node-id verify-node` |
| UI source | the reference client embedded in the binary — **superseded**, see § 1 |
| Relay backend | `--data-dir` (durable disk) |
| Provider | a stub ACP agent speaking JSON-RPC, because the built-in Pi adapter is not needed for this socket-path check |

The provider stub is worth stating plainly: the worker role now drives ACP, and
the stub speaks the same ACP JSON-RPC requests and `session/update` notifications
as a native agent. The stub is only there to stand in for the agent binary; the
dispatch, relay, report and replay path under test is the production one.

```bash
#!/usr/bin/env bash
# Minimal ACP agent: answer initialize/session/new, then emit two ACP text
# updates for session/prompt and acknowledge the prompt.
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([^,]*\),"method":.*/\1/p')
  method=$(printf '%s' "$line" | sed -n 's/.*"method":"\([^"]*\)".*/\1/p')
  case "$method" in
    initialize)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":1,"agentCapabilities":{"loadSession":true}}}\n' "$id"
      ;;
    session/new|session/load)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"verify-session"}}\n' "$id"
      ;;
    session/prompt)
      printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"verify-session","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"pong "}}}}'
      printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"verify-session","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"from fake ACP"}}}}'
      printf '{"jsonrpc":"2.0","id":%s,"result":{"stopReason":"end_turn"}}\n' "$id"
      ;;
  esac
done
```

## 1. Server, worker, UI, dispatch

The two startup commands, with a release build of the binary:

```bash
target/release/loom server \
  --bind 127.0.0.1:38899 --data-dir …/verify/server --node-id verify-node

target/release/loom worker --server-url http://127.0.0.1:38899 \
  --name verify-machine --state …/verify/machine/host-id \
  --provider-cmd …/verify/fake-acp.sh
```

The commands above are the ones a rerun uses. The recorded output below is from
the run as it happened, when the two roles were two files — so the log lines it
quotes keep the older process names, which the one binary still prints.

Recorded output:

```
### 1. start server (server-only), loopback + durable local log
  health:  {"status":"ok","protocol_version":3,"node_id":"verify-node","uptime_ms":11,"readers":8,"retained_events":0}
  version: {"version":"0.1.0","protocol_version":3}
  server log: loom-server (server-only) listening on http://127.0.0.1:38899 (node verify-node, no local worker)

### 2. join a worker (worker-only, outbound)
  worker log: loom-worker "verify-machine" enrolled as host_01M289QB02TM9P5HGJK0SHF7K9 with http://127.0.0.1:38899
  hosts:      {"count":1,"hosts":[{"id":"host_01M289QB02TM9P5HGJK0SHF7K9","name":"verify-machine","status":"connected"}]}
  primary:    {"source":"remote","host":"verify-machine"}
  host-id file: host_01M289QB02TM9P5HGJK0SHF7K9

### 3. open the UI from the same origin as the API
  GET /            -> 200 text/html; charset=utf-8
  GET /app.js      -> 200 text/javascript; charset=utf-8

### 4. dispatch a task: create a thread, message it
  thread: thr_01M289QB4AA77GCPJPHQXRHQFB
  {"thread_id":"thr_01M289QB4AA77GCPJPHQXRHQFB","events":["thread_message_added","thread_status_changed"]}

### 5. the run's events land on thread:{id}, replayable (subscribe-then-replay)
  {"type":"thread_message_added","...":…}
  {"type":"thread_status_changed","...":…}
  {"type":"thread_run_event","event":"started",  …}
  {"type":"thread_run_event","event":"turn",     …}
  {"type":"thread_run_event","event":"output",   "text":"pong "}
  {"type":"thread_run_event","event":"output",   "text":"from fakepi"}
  {"type":"thread_run_event","event":"turn",     …}
  {"type":"thread_run_event","event":"finished", "outcome":"completed"}
  {"type":"thread_status_changed","...":…}
  runs: {"runs":[]}
```

What this proves, item by item:

- The server answers `/health` and `/api/v1/version` with `protocol_version: 3`,
  and its startup line says **server-only, no local worker** — it did not wait
  for or start one.
- The worker enrolled as `host_01M…`, and the same id is in the state file. The
  host appears `connected`; primary-host resolution returns `source: "remote"`
  even though the worker is on the same host, because the server declared no
  local host — the server-only degradation path.
- The UI and `/app.js` were served by the server from the API's origin. This
  line is **historical**, see the note below.
- Posting a message moved the thread to `working` and dispatched a run; the
  provider's output arrived as `thread_run_event` frames on `thread:{id}`,
  **through the relay**, and the run reached exactly one terminal event
  (`finished`, `completed`). The thread then left `working`.
- `GET /api/v1/runs` is empty after the terminal event: the run was reaped, not
  left in flight.

> **Superseded: how the UI is served.** Step 3 above ran against the buildless
> reference client, which was compiled into the binary with `include_bytes!` and
> was the default UI. That client is still gone, and the product app has taken
> its place *inside* the same binary: `crates/server/build.rs` embeds
> `apps/app/dist` and the server role serves it, so there is nothing to configure —
> `LOOM_UI_DIR`, the variable the bundle-on-disk shape used, is simply not read
> any more. The recorded command in this step named it, so a rerun starts the
> server without it and checks the served shell and its `/assets/*.js` and
> `/assets/*.css` rather than `/app.js` and `/style.css`
> ([`ui.md`](ui.md), [`releasing.md`](releasing.md)). Everything else this run
> recorded — server-only startup, enrollment, dispatch, relay replay and the
> restart claims in § 2 — is unaffected by that change.

## 2. Restart: replay window and host identity

Commands from `docs/upgrades.md` § What a restart does not lose. The one binary,
restarting the server against the same `--data-dir` and the worker against the
same `--state` file.

```bash
# after the first turn, kill the server and start it again with the same
# --data-dir, then restart the worker with the same --state file
```

Recorded output:

```
### first boot
  host id: host_01M289RFSNQT4YMSGKE1NPZ3G1
  thread:  thr_01M289RG1F7FCQFCHRT99F85KH
  retained frames on thread scope: 10
  log files: shard-0.log shard-1.log shard-2.log shard-3.log shard-4.log shard-5.log shard-6.log shard-7.log

### restart the server (same --data-dir), restart the worker (same --state file)
  replay after server restart: 10 frames (was 10)
  replay window survived the restart: yes
  hosts after worker re-enroll: {"count":1,"ids":["host_01M289RFSNQT4YMSGKE1NPZ3G1"]}
  worker identity reused, no second host: yes
```

What this proves:

- The durable backend really is durable: all eight shard logs were on disk, and
  a server restart replayed the identical 10 frames for the thread scope.
- The worker re-enrolled as the **same** host id from its state file; the host
  list holds one machine, not two. This is the property `upgrades.md` relies on
  for a safe restart.

## What this run does not cover

Honest boundaries, so the next run knows where to start:

- **The real `pi` provider.** The embedded `pi-acp` path is exercised by the
  repository's `crates/worker/tests/acp_embedded.rs`; this run stood in a stub
  for the ACP agent binary.
- **Remote access.** Tailscale Serve and the reverse-proxy path in
  [`remote-access.md`](remote-access.md) are configuration, not code; they were
  not exercised here. The relevant host-side invariant is checkable anywhere:
  `ss -ltnp | grep 38886` must show `127.0.0.1:38886`, never `0.0.0.0:38886`.
- **The Node execution plane.** bb's `apps/host-daemon` is not checked in yet;
  the worker role was verified as the reference implementation of the same
  contract.
- **Worker self-update.** This run predates it. The acceptance scenario from
  [`upgrades.md`](upgrades.md) — *protocol mismatch → update → reconnect → the
  run is handled correctly* — is `crates/worker/tests/self_update.rs`, which runs
  a real worker process against a fake newer-protocol server and then against a
  real server, and which CI runs as its own `worker self-update end to end` job
  ([`ci.md`](ci.md#the-self-update-job)).

There is no longer an installer or a shipped unit to verify: the CLI is the
interface, and `loom server --version` / `loom worker --version` print the exact
release line the packaging checks ([`releasing.md`](releasing.md)).
