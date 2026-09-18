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
| UI source | the reference client embedded in the binary — **superseded**, see § 1 |
| Relay backend | `LOOM_DATA_DIR` (durable disk) |
| Provider | a stub ACP agent speaking JSON-RPC, because the built-in Pi adapter is not needed for this socket-path check |

The provider stub is worth stating plainly: `loom-daemon` now drives ACP, and
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

## 1. Server, daemon, UI, dispatch

Commands from `deploy/README.md` § Quick start and `docs/provider-protocol.md`
§ Running it, with the release binaries.

```bash
LOOM_BIND=127.0.0.1:38899 LOOM_DATA_DIR=…/verify/server LOOM_NODE_ID=verify-node \
  target/release/loom-server

target/release/loom-daemon --server-url http://127.0.0.1:38899 \
  --name verify-machine --state …/verify/machine/host-id \
  --provider-cmd …/verify/fake-acp.sh
```

Recorded output:

```
### 1. start server (server-only), loopback + durable local log
  health:  {"status":"ok","protocol_version":3,"node_id":"verify-node","uptime_ms":11,"readers":8,"retained_events":0}
  version: {"version":"0.1.0","protocol_version":3}
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

- The server answers `/health` and `/api/v1/version` with `protocol_version: 3`,
  and its startup line says **server-only, no local daemon** — it did not wait
  for or start one.
- The daemon enrolled as `host_01M…`, and the same id is in the state file. The
  host appears `connected`; primary-host resolution returns `source: "remote"`
  even though the daemon is on the same host, because the server declared no
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
> `apps/app/dist` and `loom-server` serves it, so there is nothing to configure —
> while `LOOM_UI_DIR`, the variable the bundle-on-disk release used, now makes
> the server exit with an error naming the removal. So the recorded command in
> this step would fail before it reached `GET /`: a rerun starts the server with
> no UI variable at all and checks the served shell and its `/assets/*.js` and
> `/assets/*.css` rather than `/app.js` and `/style.css`
> ([`ui.md`](ui.md), [`releasing.md`](releasing.md)). Everything else this run
> recorded — server-only startup, enrollment, dispatch, relay replay and the
> restart claims in § 2 — is unaffected by that change.

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

## 4. Installing from a release

The path in [`../deploy/README.md`](../deploy/README.md) § Install from a
release, run on a machine with neither a Rust toolchain nor a checkout. There is
no release yet (R1 is separate work), so the assets were staged on a local
server that serves GitHub's URL and JSON shapes; the parts that are GitHub's
behaviour rather than ours were then checked against real GitHub, read-only.

| | |
| --- | --- |
| Server | the machine that ran the control plane, `127.0.0.1:38911` |
| Clean machine | `alpine:3.20` amd64 container: no `cargo`/`rustc`, `deploy/` obtained from the release archive |
| Artifacts | `loom-server-x86_64-unknown-linux-musl` 6.4 MB, `loom-daemon-x86_64-unknown-linux-musl` 2.4 MB (both static), `SHA256SUMS`, `loom-0.1.0-x86_64-unknown-linux-musl.tar.gz` |

```bash
# on the clean machine: the archive, then one install command
curl -fsSLO "$RELEASE/v0.1.0/loom-0.1.0-x86_64-unknown-linux-musl.tar.gz"
tar xzf loom-0.1.0-x86_64-unknown-linux-musl.tar.gz
cd loom-0.1.0-x86_64-unknown-linux-musl
deploy/install.sh --release v0.1.0 daemon builder-1 http://127.0.0.1:38911
```

Recorded output (elisions marked `…`):

```
-- no Rust toolchain here: cargo/rustc absent
loom-0.1.0-x86_64-unknown-linux-musl.tar.gz: OK
  downloading 550W-HOST/loom release v0.1.0 for x86_64-unknown-linux-musl
  verified loom-server-x86_64-unknown-linux-musl 56a02f89ecacce90ec4725a5c66d15eb31090b9eade56feda67e6fdefc208f4c
  verified loom-daemon-x86_64-unknown-linux-musl 0ff2d819b243670b643367020b5a36a4b3f0dcbaa2f07828bb453cb6d4463109
  installed binaries to /usr/local/bin
  …
  created /etc/loom/daemon/builder-1.env (server http://127.0.0.1:38911, host name amax)
  …
enrolled host id: host_01M29ZJ90WEXJKKCJSY72K5HJG
server sees:      {"hosts":[{"id":"host_01M29ZJ90WEXJKKCJSY72K5HJG","name":"amax","kind":"persistent","status":"connected",…}]}
daemon log:       loom-daemon "amax" enrolled as host_01M29ZJ90WEXJKKCJSY72K5HJG with http://127.0.0.1:38911
```

What this proves, item by item:

- `cargo`/`rustc` absent, and nothing on the machine but the archive: the install
  needed no toolchain and no checkout, only `deploy/` from the release and the
  network.
- The archive's own SHA-256 was checked against the release's `SHA256SUMS`
  (`…: OK`) before it was unpacked — that step is the operator's, the installer
  checks the binaries it fetches itself.
- The installer named the assets it fetched and the digests it checked, then
  installed `/usr/local/bin/loom-{server,daemon}`; both hashes equal the
  published ones.
- The daemon enrolled as `host_01M29Z…`, and the server reported **that** host id
  `connected` while the container was still running: not just installed, joined.

Failure modes, against the same staged release:

| Scenario | Recorded result |
| --- | --- |
| digest does not match | `SHA-256 mismatch for loom-server-…: SHA256SUMS says dead2f89…, the download is 56a02f89…`, non-zero exit, pre-existing `/usr/local/bin/loom-server` byte-identical, daemon never fetched |
| asset not in the release | `cannot download <url> — set GITHUB_TOKEN if … is private`, nothing installed |
| unpublished architecture (`armv7l`) | `no release binary for machine type armv7l: …`, no download attempted |
| non-Linux host | `release binaries are Linux-only`, no download attempted |
| `curl` absent / `sha256sum` absent / both absent | downloaded with `wget` / verified with `shasum` / `needs curl or wget` |
| install re-run | environment files and data directories unchanged, binaries re-verified |

GitHub behaviour, checked against real GitHub on a release of another private
repository: the asset-id lookup against a real release object (the pipeline
returns the asset id, not the uploader's nested `id`), `releases/latest`, and an
authenticated `application/octet-stream` download of a private asset verifying
against that release's `SHA256SUMS`. `github.com/…/releases/download/…` answers
`404` for a private repository even with a token, which is why `--release` goes
through the API whenever `GITHUB_TOKEN` is set.

## What this run does not cover

Honest boundaries, so the next run knows where to start:

- **systemd itself.** The units were parsed by `systemd-analyze` (systemd 245),
  not started. `install.sh`'s root-only steps (`useradd`, `install`, `systemctl
  enable --now`) were not executed. A follow-up on a real VM should run
  `deploy/install.sh all` and `systemctl status`.
- **The real `pi` provider.** The embedded `pi-acp` path is exercised by the
  repository's `crates/daemon/tests/acp_embedded.rs`; this run stood in a stub
  for the ACP agent binary.
- **Remote access.** Tailscale Serve and the reverse-proxy path in
  [`remote-access.md`](remote-access.md) are configuration, not code; they were
  not exercised here. The relevant host-side invariant is checkable anywhere:
  `ss -ltnp | grep 38886` must show `127.0.0.1:38886`, never `0.0.0.0:38886`.
- **The Node execution plane.** bb's `apps/host-daemon` is not checked in yet;
  `loom-daemon` was verified as the reference implementation of the same
  contract.
- **Daemon self-update.** This run predates it. The acceptance scenario from
  [`upgrades.md`](upgrades.md) — *protocol mismatch → update → reconnect → the
  run is handled correctly* — is `crates/daemon/tests/self_update.rs`, which runs
  a real daemon process against a fake newer-protocol server and then against a
  real server, and which CI runs as its own `daemon self-update end to end` job
  ([`ci.md`](ci.md#the-self-update-job)). What is not covered anywhere yet is
  systemd actually restarting the new binary after the update exits, for the
  same reason as `systemd itself` above.
