# Upgrading

loom ships two coordinated binaries. The server and daemon negotiate one
internal protocol number; the client the server carries uses the separately
exported bb public schema and WebSocket subprotocol:

| Artifact | What it is | Where it runs |
| --- | --- | --- |
| **server** | `loom-server` binary, with the product app from `apps/app` compiled into it | one machine |
| **daemon** | `loom-daemon` binary | every execution machine |

The client is part of the server binary, not a directory beside it
([`ui.md`](ui.md)): installing or upgrading the server installs or upgrades its
UI, there is no bundle to place, and no server can serve a client other than the
one it was built with. An environment file written while main served a
bundle from disk still names `LOOM_UI_DIR` — never a release, since no release
carried that shape — and the server now exits at startup with an error naming
the removal rather than ignoring it: that line is the one thing such an
environment has to drop. `LOOM_UI_PROXY` — development only, a frontend
dev server to reverse-proxy to — is the single override that survives, and no
deployment uses it.

This page defines what "agree" means, how the daemon follows a server upgrade on
its own, and how to roll back. The guiding lesson is bb #3143: when a self-update
replaces the process that owns a daemon's transport, every in-flight turn dies.
loom's answer is structural — the server and daemon are independent processes,
delivery is a replayable log, and the update is a supervised restart rather than
an in-process swap — and this page is how that is used rather than fought.

## Version consistency

Two version fields exist, and only one of them is a compatibility gate:

- **`protocol_version`** — the internal daemon wire contract on `/internal/ws`
  and for `RunDispatch`/`ProviderReport`. Currently `3`
  (`crates/server/src/lib.rs`, bumped for the public/internal WebSocket split).
  **This is the daemon compatibility gate.**
- **`loom-bb-realtime-v1`** — the explicit public `/ws` subprotocol. Its message
  shapes are validated against `contracts/bb/client-ws.json`; it is not selected
  from `Origin`.
- **`version`** — the crate semver (`0.1.0`). Informational; releases with the
  same `protocol_version` are interoperable regardless of `version`.

`GET /api/v1/version` reports both:

```bash
curl -s http://127.0.0.1:38886/api/v1/version
# {"version":"0.1.0","protocol_version":3}
```

The server sends `protocol_version` in the first `hello` frame on
`/internal/ws`. A v3 daemon reads it before enrolling and refuses a mismatch
(`loom_daemon::ensure_compatible_protocol`). During the v2 to v3 transition, an
old daemon still dials `/ws` without a WebSocket subprotocol; the server sends
one legacy `welcome` carrying v3 and closes, which drives that daemon into the
same self-update flow. A public client must negotiate `loom-bb-realtime-v1` and
never sees either internal handshake.

Both binaries answer the same question about the file itself, before either one
has been started — which is what a download has to be checked with
([`releasing.md`](releasing.md)):

```bash
loom-server --version
# loom-server 0.1.0 (x86_64-unknown-linux-musl, protocol 3, commit 0f1e2d3c…)
```

The line names the target triple and the commit the file was built from as well
as the version, so two binaries from different releases are told apart without
starting either of them.

> **Rule:** server and daemon must speak the same internal `protocol_version`,
> and the client inside the server must match the server's exported public
> schema. For a protocol bump, deploy the server first: old daemons receive the
> migration mismatch and pull the matching binary before they can enroll.

A mismatch is not a degraded mode. The daemon refuses to enroll, and — with
self-update enabled, which is the default — installs the server's own daemon and
restarts. With self-update disabled the refusal is loud and permanent until an
operator intervenes; that is deliberate, not a bug.

### Checking a fleet

```bash
# the server's contract
curl -s http://127.0.0.1:38886/api/v1/version | grep protocol_version

# what the server hosts for daemons
curl -s http://127.0.0.1:38886/install/version
# {"version":"0.1.0","protocolVersion":3}

# each daemon reports the number it speaks at startup, and a mismatch in its log
journalctl -u 'loom-host-daemon@builder-1' | grep -i 'protocol version'
```

## Daemon self-update

The default is that a daemon **pulls**: it asks the server on connect and decides
for itself whether it needs to update. There is no push.

### Why pull, not push

The question the issue left open — may the server *tell* a daemon to update, or
does the daemon check on every connect? — is settled in favour of pull:

- **All the state is already in the handshake.** The server's protocol version
  is the first frame of every connection. A pull design reads no new state and
  needs no new frame; a push design needs the server to remember, per host,
  which version it last told that host to install, and to re-send after its own
  restart.
- **The server never tracks who is current.** With pull there is no per-host
  update table to persist, reconcile, or get wrong. A daemon that has been down
  for a week behaves exactly like one that just reconnected.
- **Convergence is bounded by reconnect, which is already the failure model.**
  A daemon reconnects continuously (`Restart=always`, and the session loop's own
  backoff), so it converges on the first connect after the upgrade. A push
  reaches a *connected* daemon sooner, but only a connected one — the offline
  machine still needs the pull path. Push would therefore be an optimisation for
  a case that is already fast, at the cost of a stateful protocol.
- **Nothing in loom can execute the update on the server's behalf.** The server
  does not start, supervise, or signal daemons (`docs/process-model.md`), so a
  push would end in "send a frame that asks the daemon to do what it would have
  done anyway".

The cost is honest and accepted: a daemon whose server upgrade happened while it
was connected learns on its next reconnect rather than immediately. In practice
that is the reconnection after the refusal, which is the same moment. If
convergence ever needs to be faster than reconnect, a push frame can be added
later without removing the pull path — the daemon's decision is local either
way, and this page is where the reason would be recorded.

### The flow

A protocol mismatch on connect now ends in a restart, not in a permanent
failure:

```text
  deployed v2 daemon                    server (protocol 3)
    │                                         │
    │── dial /ws (no subprotocol) ──────────▶│
    │◀─ legacy welcome {protocol_version:3} ─│
    │◀─ close ───────────────────────────────│
    │                                         │
    │  ensure_compatible_protocol(3)
    │    local is 2 → ProtocolMismatch        │
    │  (nothing enrolled; no dispatch read)   │
    │                                         │
    │── GET /install/version ────────────────▶│
    │◀─ {version, protocolVersion:3} ─────────│
    │── GET /install/loom-daemon ────────────▶│
    │   ?target=<this binary's triple>        │
    │   If-None-Match: "sha256-<installed>"   │
    │◀─ 200 + X-Loom-Artifact-Sha256: <d> ───│
    │                                         │
    │  verify digest; fsync; chmod 0755;      │
    │  atomically rename; record digest       │
    │  exit 0                                 │
    ▼                                         │
  systemd starts the v3 binary ───────────────┘
    │
    │── dial /internal/ws ──────────────────▶│
    │◀─ hello {protocol_version:3} ──────────│
    │  match → enroll → subscribe → replay from the persisted cursor
    ▼
```

### What the server hosts

| Route | Answer |
| --- | --- |
| `GET /install/version` | `{"version":"0.1.0","protocolVersion":3}` |
| `GET /install/loom-daemon?target=<triple>` | the binary, with `X-Loom-Artifact-Sha256` and `ETag` |

The server looks in `LOOM_ARTIFACT_DIR`, and **by default in the directory
holding the running `loom-server`**. That default is what makes an ordinary
deployment work with no configuration: `deploy/install.sh` installs
`/usr/local/bin/loom-server` and `/usr/local/bin/loom-daemon` side by side, so
the sibling daemon *is* the artifact that matches this server. A release archive
extracted and installed the same way behaves identically. Two file names are
accepted:

- `loom-daemon-<triple>` — the release page's asset name, which is how a server
  is given binaries for machines of another architecture;
- `loom-daemon` — the installed name, and only an answer for the server's **own**
  triple, because a binary built for the wrong machine would install and then
  fail to execute.

The `target` query parameter is validated before it touches the filesystem
(ASCII alphanumerics, `-` and `_` only), so a crafted value cannot walk out of
the directory.

**Authentication: none**, like every other route. The control plane has no
authentication layer and the network boundary is the security model
([`remote-access.md`](remote-access.md)); serving the daemon adds no exposure the
API did not already have, since the API already dispatches arbitrary command
execution to every enrolled machine, and the bytes are public software. Whoever
can reach the port can already enroll a host.

A containerised server has no sibling `loom-daemon` — the image is `scratch` and
carries the control plane only. Point `LOOM_ARTIFACT_DIR` at a directory
containing `loom-daemon-<triple>` if a containerised server should host
artifacts, or run daemons from images (`docs/containers.md`) and update them by
pulling a new image, which is the same "replace the file, restart the process"
with the container runtime as the supervisor.

### Verification: digest, and what it does not cover

The daemon recomputes SHA-256 over the bytes it received and refuses to install
unless it equals the `X-Loom-Artifact-Sha256` the server sent. That defeats a
truncated download, a corrupted one, and a proxy that rewrote the body. A
mismatch is a failure like any other: the current binary keeps running and the
next attempt is scheduled.

It is **not** a signature. The same server serves the artifact and the digest, so
a compromised server can serve both. That trust root is not new — the control
plane already tells that machine what to execute — and the digest's job here is
integrity in transit plus a stable identity for conditional requests, which is
exactly what it does. Moving the trust root (minisign, sigstore, a digest pinned
in release notes) is the natural next step if artifacts ever come from anywhere
other than the server that dispatches the work, and
[`releasing.md`](releasing.md) § What is not here records that as an open item.

The **existing binary is never touched before the replacement is complete**:
the bytes are written to a temporary file in the install directory, fsynced, made
executable, and only then `rename`d over the target. A rename within one
directory is atomic, and it does not disturb the running process, whose image is
already mapped. An interrupted download, a failed digest, a full disk and a crash
between the two steps all leave the old binary in place and working.

### In-flight runs

The update is only attempted on a connection that **never enrolled**: the
protocol refusal happens on the internal `hello` frame (or the temporary v2
migration `welcome`), before `enroll_host` and before
a single dispatch is read. So the process that performs an update has no run in
flight on that connection.

A run that was in flight on the *previous* connection is an ordinary disconnect,
unchanged by self-update:

- the provider process dies with the old daemon process, and the server's reaper
  ends the run — `host_stale` after the host stops heartbeating, or `timed_out`
  past the deadline — so the thread leaves `working`;
- the run is **not** resumed. Its dispatch was published before the daemon's
  persisted cursor, so the replay on reconnect does not resend it. Re-issue the
  turn.

That is the same guarantee `docs/provider-protocol.md` makes for a daemon crash,
and it is why the update does not need a quiesce protocol: it cannot interrupt a
run that this process is executing, and a run it did not execute is reaped by the
server either way. If a fleet upgrade must not lose any work at all, drain the
daemons first (`systemctl stop`, so in-flight runs are reaped and nothing new is
dispatched) and start them after the server upgrade — a policy choice, not a
requirement of the mechanism.

### Failure modes and the backoff

Every failure keeps the current daemon running and retries:

| Failure | What happens |
| --- | --- |
| `/install/version` unreachable or not JSON | keep running, retry on the update backoff |
| no artifact for this triple | keep running, retry; the server logs nothing per-request |
| download truncated / digest mismatch | refuse to install, keep running, retry |
| install directory not writable | refuse to install, keep running, retry; the error names the directory |
| the installed binary still speaks the old protocol | the next connect is refused again, the attempt count increments, and the delay grows |
| server is **older** than the daemon | no update is attempted at all; downgrade is refused by design |

The schedule is **5 s doubling to 5 minutes**, counted per target protocol
version and persisted in `<state-dir>/host-daemon-update-attempt.json`, next to
the host id. Persisting it is what distinguishes "backing off" from "crash
looping": a daemon restarted by `Restart=always` reads the count and waits its
turn rather than hammering the server. A successful install writes the digest to
`<state-dir>/host-artifact.sha256`, which is also what makes the *next* attempt
conditional.

There is deliberately **no state in which the daemon stops retrying**. The worst
case is a bounded retry every five minutes and a log line each time.

### Disabling self-update

An operator who wants to control upgrades centrally turns it off:

```bash
# the flag, or the environment
loom-daemon --server-url https://loom.example.com --no-auto-update
```

```
# /etc/loom/daemon/<server-key>.env
LOOM_AUTO_UPDATE=0
```

The reason is logged at startup, so the journal says *why* a mismatched daemon is
not following its server:

```
loom-daemon self-update: disabled by configuration; a server that speaks a newer
protocol will be refused and retried, never fetched
```

With it off, a protocol mismatch is retried on the connection backoff (1 s
doubling to 30 s) and never fetched — the process stays up, keeps its binary, and
the refusal is logged on every attempt. bb spells the affirmative flag
`--auto-update`; loom accepts both spellings and defaults it **on**, because a
daemon that cannot follow a server upgrade is the operational trap this exists to
remove.

### The supervisor is required

The daemon exits after a successful install and something must start the new
file:

```ini
# deploy/systemd/loom-host-daemon@.service
Restart=always
RestartSec=5s
```

`Restart=always` is what `deploy/install.sh` already installs, and the container
images use `restart: unless-stopped`. A daemon run bare from a shell has no
supervisor, and after an update it simply exits — run it under systemd, a
container, or any process manager that restarts. The exit status is **0**, so a
planned update is not recorded as a failure.

## Updating by hand

`install.sh` plus a restart remains the supported path for the server, and for a
daemon with self-update disabled:

```bash
# A. from a checkout: build, then install from the build output
cargo build --release
sudo deploy/install.sh server

# B. from a release: no toolchain needed, the installer downloads and verifies
sudo deploy/install.sh --release v0.2.0 server

# ...then, either way, on the server machine
sudo systemctl restart loom-server

# ...on each execution machine with self-update disabled:
sudo deploy/install.sh --release v0.2.0 daemon builder-1 https://loom.example.com
sudo systemctl restart loom-host-daemon@builder-1
```

With self-update enabled the daemon steps are unnecessary: restarting the server
is the whole upgrade. `--release <version>` downloads the binaries for this
machine's target from the GitHub Release, checks each of them against the
release's `SHA256SUMS`, and only then replaces `/usr/local/bin/loom-*`;
[`../deploy/README.md`](../deploy/README.md) § Install from a release has the
details, including `GITHUB_TOKEN` for a private repository. A download that fails,
or one whose digest does not match, aborts **before** anything is installed and
exits non-zero, so a fleet upgrade is never half-done by a bad connection.

`install.sh` never overwrites an existing environment file, so re-running is safe
and safe from a configuration-management tool that replaces binaries.

### What a restart does not lose

- **The daemon's identity.** The host id is persisted in `LOOM_DAEMON_STATE` on
  first enroll and re-presented on start, so a restart — including the restart
  after a self-update — updates the existing host instead of enrolling a second
  one. A host is "a machine, not a connection".
- **The replay window.** With `LOOM_DATA_DIR` the log is on disk and survives a
  server restart; with `LOOM_REDIS_URL` it is shared and also lets a second node
  attach to the same window. Only the default in-process backend loses it.
- **Missed frames.** A daemon persists its host-scope cursor next to its host id
  and, on start, subscribes *then* replays from that cursor. A dispatch published
  while it was restarting arrives late rather than being lost, and the event-id
  dedup set drops the overlap.
- **In-flight runs are not lost, they are reaped.** A run whose daemon restarted
  mid-flight is completed by the server's reaper (`timed_out` after the deadline,
  or `host_stale` after the host stops heartbeating), so a thread never stays
  `working` forever. The provider work itself is not resumed; re-issue the turn.

## Rollback

Rolling back is running the previous binaries and restarting; data formats are
stable within a `protocol_version`.

```bash
# keep the previous binaries where the upgrade can find them again
sudo install -m 0755 /var/lib/loom/bin/loom-server.prev /usr/local/bin/loom-server
sudo systemctl restart loom-server

# a daemon likewise
sudo install -m 0755 /var/lib/loom/bin/loom-daemon.prev /usr/local/bin/loom-daemon
sudo systemctl restart loom-host-daemon@builder-1
```

If the previous binaries were not kept, the previous release is the copy:

```bash
sudo deploy/install.sh --release v0.1.0 server
sudo systemctl restart loom-server
```

Rules:

- Roll back **both ends together** when a `protocol_version` change is involved.
  A new server with old daemons is the mismatch the handshake refuses; the old
  daemons will not silently follow a newer server once it is rolled back, because
  a downgrade is refused — install the old binary by hand.
- **Self-update cannot undo an upgrade by itself.** A daemon never installs a
  binary for a protocol older than its own. Rolling a daemon back is
  `install.sh` (or copying the previous file) plus a restart.
- The environment file and the data directory are unchanged across an ordinary
  upgrade, so rollback does not touch them. One commit range changed how
  the UI is served rather than how the protocol works: the server that serves a
  bundle from disk needs `LOOM_UI_DIR` plus that directory, and the server that
  carries the client in its binary refuses to start while the variable is set,
  so rolling across that boundary edits the environment file in one direction or
  the other. (The disk-bundle shape existed only between two commits on main; no
  release shipped it.) The two update state files
  (`host-daemon-update-attempt.json`, `host-artifact.sha256`) are safe to delete:
  the next attempt is then unconditional and unthrottled.
- The relay log needs no migration within a `protocol_version`. It is an
  append-only per-shard file (or Redis streams) that both the old and the new
  binary read with the same framing. If a future release changes that framing, it
  will bump `protocol_version` and this page will say so.

## Why this is not a package manager

There is no version directory, no atomically-swapped release tree, and no
`loom upgrade` command. What exists is one exact mechanism: a daemon may replace
its own executable with one the server hosts, and exit for a restart. Those are
the shapes the fork explicitly does not require
(`architecture.md` § Deployment shapes): bare systemd must be enough. Nix,
Ansible, a container image or a package repository can all be layered on top —
they all reduce to "put the binary here, write the environment file, restart the
unit", which is exactly what `install.sh` does, and the daemon's own path is the
same three steps with the server as the source.
