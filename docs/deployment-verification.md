# Deployment verification

One recorded clean-machine run of the path in
[`../deploy/README.md`](../deploy/README.md): start the server, join a daemon,
open the UI, dispatch a task, and confirm the run's frames replay. It exists so
the deployment documents are not claims — every command below is the command in
the guide.

The goal is the acceptance path, not a benchmark: server-only startup, a
daemon-only process on the same host joining over loopback, the UI served from
the API's origin, a thread turn dispatched through the relay, and the run's
events replayable from the retained window. A second run covers the restart
claims in [`upgrades.md`](upgrades.md).

## Environment

| | |
| --- | --- |
| Host | Ubuntu 20.04.6 LTS, x86_64, kernel 5.15 (clean workspace) |
| Revision | `a0f8730` + this change set |
| Toolchain | `rustc 1.98.0`, build profile `release` (`cargo build --release`) |
| Binaries | `target/release/loom-server`, `target/release/loom-daemon` |
| Server bind | `127.0.0.1:38899` (a test port; the unit default is `38886`) |
| Relay backend | `LOOM_DATA_DIR` (durable disk) |
| Provider | a stub emitting real Pi RPC frames, because `pi` is not installed here |

The provider stub is worth stating plainly: `loom-daemon`'s Pi bridge is what
the daemon runs, and the stub speaks the same JSONL frames as
`pi --mode rpc`. The stub is only there to stand in for the agent binary; the
dispatch, relay, report and replay path under test is the production one.

```bash
#!/usr/bin/env bash
# Minimal Pi RPC provider: read the prompt, emit two assistant deltas, settle.
read -r _prompt || true
printf '%s\n' '{"type":"agent_start"}'
printf '%s\n' '{"type":"turn_start"}'
printf '%s\n' '{"type":"message_update","assistantMessageEvent":{"type":"text_delta","delta":"pong "}}'
printf '%s\n' '{"type":"message_update","assistantMessageEvent":{"type":"text_delta","delta":"from fakepi"}}'
printf '%s\n' '{"type":"turn_end"}'
printf '%s\n' '{"type":"agent_settled"}'
```

## 1. Server, daemon, UI, dispatch

Commands from `deploy/README.md` § Quick start and `docs/provider-protocol.md`
§ Running it, with the release binaries.

```bash
LOOM_BIND=127.0.0.1:38899 LOOM_DATA_DIR=…/verify/server LOOM_NODE_ID=verify-node \
  target/release/loom-server

target/release/loom-daemon --server-url http://127.0.0.1:38899 \
  --name verify-machine --state …/verify/machine/host-id \
  --session-dir …/verify/machine/sessions --provider-cmd …/verify/fakepi.sh
```

Recorded output:

```
### 1. start server (server-only), loopback + durable local log
  health:  {"status":"ok","protocol_version":1,"node_id":"verify-node","uptime_ms":11,"readers":8,"retained_events":0}
  version: {"version":"0.1.0","protocol_version":1}
  server log: loom-server (server-only) listening on http://127.0.0.1:38899 (node verify-node, no local daemon)

### 2. join a daemon (daemon-only, outbound)
  daemon log: loom-daemon "verify-machine" enrolled as host_01M289QB02TM9P5HGJK0SHF7K9 with http://127.0.0.1:38899
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

- The server answers `/health` and `/api/v1/version` with `protocol_version: 1`,
  and its startup line says **server-only, no local daemon** — it did not wait
  for or start one.
- The daemon enrolled as `host_01M…`, and the same id is in the state file. The
  host appears `connected`; primary-host resolution returns `source: "remote"`
  even though the daemon is on the same host, because the server declared no
  local host — the server-only degradation path.
- The UI and `/app.js` are served by the server from the API's origin.
- Posting a message moved the thread to `working` and dispatched a run; the
  provider's output arrived as `thread_run_event` frames on `thread:{id}`,
  **through the relay**, and the run reached exactly one terminal event
  (`finished`, `completed`). The thread then left `working`.
- `GET /api/v1/runs` is empty after the terminal event: the run was reaped, not
  left in flight.

## 2. Restart: replay window and host identity

Commands from `docs/upgrades.md` § What a restart does not lose. Same binaries,
restarting the server against the same `LOOM_DATA_DIR` and the daemon against
the same state file.

```bash
# after the first turn, kill the server and start it again with the same
# LOOM_DATA_DIR, then restart the daemon with the same --state file
```

Recorded output:

```
### first boot
  host id: host_01M289RFSNQT4YMSGKE1NPZ3G1
  thread:  thr_01M289RG1F7FCQFCHRT99F85KH
  retained frames on thread scope: 10
  log files: shard-0.log shard-1.log shard-2.log shard-3.log shard-4.log shard-5.log shard-6.log shard-7.log

### restart the server (same LOOM_DATA_DIR), restart the daemon (same state file)
  replay after server restart: 10 frames (was 10)
  replay window survived the restart: yes
  hosts after daemon re-enroll: {"count":1,"ids":["host_01M289RFSNQT4YMSGKE1NPZ3G1"]}
  daemon identity reused, no second host: yes
```

What this proves:

- The durable backend really is durable: all eight shard logs were on disk, and
  a server restart replayed the identical 10 frames for the thread scope.
- The daemon re-enrolled as the **same** host id from its state file; the host
  list holds one machine, not two. This is the property `upgrades.md` relies on
  for a safe restart.

## 3. Deploy artifacts

The scripts and units were checked without a systemd host (this workspace is a
non-root container; installing units and enabling services needs both root and
systemd, which the production host has and the verification host does not):

```bash
$ bash -n deploy/install.sh && bash -n deploy/uninstall.sh
# both: syntax ok

$ deploy/install.sh help
Usage: install.sh <command> [arguments]     # server / daemon / all / help

$ deploy/uninstall.sh help
Usage: uninstall.sh <command> [--purge]     # server / daemon / all / binaries

$ systemd-analyze verify deploy/systemd/loom-server.service
# parsed, no findings for this unit

$ systemd-analyze verify deploy/systemd/loom-host-daemon@.service
loom-host-daemon@i.service: Command /usr/local/bin/loom-daemon is not executable: No such file or directory
# parsed; the only finding is the expected pre-install missing binary
```

## What this run does not cover

Honest boundaries, so the next run knows where to start:

- **systemd itself.** The units were parsed by `systemd-analyze` (systemd 245),
  not started. `install.sh`'s root-only steps (`useradd`, `install`, `systemctl
  enable --now`) were not executed. A follow-up on a real VM should run
  `deploy/install.sh all` and `systemctl status`.
- **The real `pi` provider.** The Pi bridge is exercised by the repository's
  own `crates/daemon/tests/provider_e2e.rs`; this run stood in a stub for the
  agent binary.
- **Remote access.** Tailscale Serve and the reverse-proxy path in
  [`remote-access.md`](remote-access.md) are configuration, not code; they were
  not exercised here. The relevant host-side invariant is checkable anywhere:
  `ss -ltnp | grep 38886` must show `127.0.0.1:38886`, never `0.0.0.0:38886`.
- **The Node execution plane.** bb's `apps/host-daemon` is not checked in yet;
  `loom-daemon` was verified as the reference implementation of the same
  contract.
