# The process model: server, worker, and the Node execution plane

This document is the boundary contract this repository commits to. It answers a
question the current code layout cannot answer on its own: **loom's control
plane is Rust, but bb's host daemon — the part that actually runs provider CLIs —
is Node.** How do the two relate?

The two are **roles of one binary** (`loom`, built by `cargo build --release -p
loom`) and *two processes*. Which role a start takes is decided by the
**subcommand** — `loom server` or `loom worker` — and nothing else about the
boundary below changed with that: no lifetime, no resource domain and no
supervision relationship was merged.

## The decision

They are two processes with one protocol between them, and neither owns the
other. The single-box opt-in below adds a supervisor relationship between the two
*processes*; it does not merge them.

- **The server role** (`loom server`) is the control plane: state, HTTP,
  WebSocket, the relay. It requires no worker and starts none by default. The one
  opt-in is `loom server --local-worker`, which starts **one** `loom worker` child
  on the same machine and supervises it; the child is still a separate process
  with its own address space and its own reconnect loop, and a server started
  without the flag is exactly the server-only role described here.
- **The worker role** (`loom worker`) is the execution plane: it runs on a
  machine, dials the server *outbound*, enrolls as a host, and executes work. It
  never starts, supervises, or requires the server to be co-located or in the same
  process tree.
- **The bb Node execution plane** (`apps/host-daemon`, checked in beside the
  Rust workspace in a later step) implements the *same* worker contract as
  `loom-worker`. `loom-worker` is the reference implementation of that
  contract: the process shell, the enrollment/heartbeat lifecycle, and the test
  harness. When the Node host daemon lands, it replaces the shell's body, not its
  boundary.

There is no FFI, no embedded Node runtime, and no shared cgroup in the default
two-process shape. A worker that cannot reach a server is a worker that
reconnects; a server with no worker is a server that serves. That one-way
dependency is what makes both deployment shapes in `architecture.md` fall out for
free.

### Why not run the Node host daemon inside the Rust server

That is exactly bb's `runFullStack`: one process tree, one lifetime, one
resource domain. It is the cause of the two behaviours this fork is removing —
closing the execution plane takes the UI with it, and one machine exhausting
itself drags the control plane along. Keeping the boundary as a network
boundary is the point, not an implementation detail.

`--local-worker` is not a step back toward that. It starts a second *process*
that systemd or a terminal could have started instead; it does not import the
execution plane into the server — no FFI, no embedded runtime, no shared address
space, and the child is killed rather than spoken to. What it does trade away is
unit-level isolation: one supervisor now owns both lifetimes, and the pair shares
a cgroup when systemd runs the server, so the two-unit shape stays the answer for
a machine that wants the control plane sandboxed on its own.

## Startup paths

Four, and the first two are the primitives; the last two are conveniences.

| Path | Command | What it starts | What it assumes |
| --- | --- | --- | --- |
| **server-only** | `loom server` | the control plane | nothing. No worker, no data dir |
| **worker-only** | `loom worker --server-url <URL>` | one execution machine | a reachable server URL |
| **single box** | `loom server --local-worker` | both, from one command | a writable data dir |
| **full stack** | a supervisor starting both as *separate children* | both | a local server URL |

The server role is server-only unless asked. It does not probe for a worker, and
it does not exit when none connects — see the test
`a_server_with_no_worker_is_up_and_a_remote_worker_becomes_primary`. The
single-box path is the one opt-in, and it is the server itself doing the
supervising: one `loom worker` child, started from the same binary, restarted
when it exits (including the `exit 0` a self-update leaves behind) and killed
with the server. The full-stack path stays the shape that can stop either side
alone: it is an outside supervisor over two independent children, which is what
two systemd units or the two compose services already are.

## The worker contract

A worker uses the versioned internal WebSocket on `GET /internal/ws`; the
browser's bb-compatible realtime protocol remains exclusively on `GET /ws`:

```json
// worker -> server (`/internal/ws`)
{"type":"enroll_host","name":"laptop"}
{"type":"enroll_host","host_id":"host_01M…","name":"laptop"}   // reconnect
{"type":"host_heartbeat","host_id":"host_01M…"}
{"type":"host_disconnect","host_id":"host_01M…"}
{"type":"run_report","report":{"host_id":"host_…","run_id":"run_…","thread_id":"thr_…","event":{…}}}
{"type":"replay","scope":{"kind":"host","id":"host_…"},"since":"01M…"}

// server -> worker
{"type":"hello","protocol_version":3}
{"type":"host_enrolled","host":{"id":"host_01M…","status":"connected",…},"event_id":"01M…"}
{"type":"host_heartbeat_ack","host_id":"host_01M…","last_seen_at_ms":1}
{"type":"host_disconnected","host_id":"host_01M…"}
{"type":"run_report_ack","run_id":"run_…","accepted":true}
{"type":"replay_complete","scope":{…},"count":3}

// server -> worker, through the relay (`host:{id}` scope)
{"type":"event","event_id":"01M…","scope":{"kind":"host","id":"host_…"},
 "payload":"<RunDispatch JSON as a string>","created_at_ms":1}
```

The dispatch and report shapes, and the terminal-state guarantee that goes with
them, are in [`provider-protocol.md`](provider-protocol.md).

Three properties the contract guarantees:

1. **Identity survives reconnects.** A worker presents the `host_id` it was
   given; the server updates that host's status rather than minting a second
   machine. This is why a host is "a machine, not a connection".
2. **Closing the socket detaches the host.** A worker that dies without a
   goodbye still produces `host_status_changed → disconnected`, so a UI can
   render the machine as gone while the server keeps serving.
3. **The server never dials the worker.** Enrollment, heartbeat and dispatch
   all ride a connection the worker opened, so workers work behind NAT.

The HTTP surface mirrors the read side for non-socket callers:
`GET /api/v1/hosts`, `GET /api/v1/hosts/primary`,
`POST /api/v1/hosts/{id}/heartbeat`, `POST /api/v1/hosts/{id}/disconnect`, and
`POST /api/v1/hosts` (with an optional `id`) for enrollment.

## Primary host degradation

bb's server falls back to the *local* worker's id file for its "primary host".
With no local worker that fallback strands file browsing and host lookups on a
machine that is intentionally absent — the `host_unavailable` failure.

loom replaces the fallback with a policy, in `loom_domain::select_primary_host`:

1. the operator-declared local host (`--local-host-id`), **only while a
   worker is attached to it**;
2. otherwise the most recently seen connected host of any kind — the primary
   falls to a remote machine;
3. otherwise `None`, an explicit "no host enrolled yet".

The resolver cannot fail, so no route can turn "this machine has no worker"
into an error. `GET /api/v1/hosts/primary` always answers `200`, with
`source: "local" | "remote" | "no_host"` so a client can render the degradation
honestly. A server-only deployment sets no local host at all and never enters
the local branch.

## Desktop shell supervision

The desktop shell is a UI plus, optionally, two process switches:

| Switch | Starts | Stopping it |
| --- | --- | --- |
| **local server** | a control-plane child (`loom server`) | the UI disconnects from that URL |
| **local execution worker** | an execution-plane child (`loom worker`) pointed at the current server URL | the host is marked disconnected; the window is untouched |

The two switches are independent. Turning the worker off must not reload the
window, because a worker is a property of the *machine* and the UI is a client
of a URL — the same reason a worker on machine B can appear in a UI served by
machine A.

## What is in this repository today

- **`loom`** — the one artifact: a single binary (`cargo build --release -p
  loom`) that carries both roles. Which role a start takes comes from the
  subcommand, `loom server` or `loom worker`; there are no role symlinks.
  Nothing starts the other role except the named opt-in:
  `loom server --local-worker`.
- **The server role** — control plane, with host enrollment, heartbeats,
  disconnects and primary-host resolution. Server-only unless `--local-worker`
  starts and supervises one worker child on the same machine.
- **The worker role** — the reference worker-only entry point. It enrolls,
  follows its `host:{id}` room through the relay with replay on reconnect, and
  runs the provider the control plane dispatches.
- `loom-provider-protocol` — the dispatch/report contract between the two. ACP
  framing and provider-specific translation stay in the worker, not in the
  control plane; the terminal-state guarantee is specified in
  [`provider-protocol.md`](provider-protocol.md).
- The bb Node sources (`apps/`, `packages/`) are **not yet checked in**; they
  land beside the Rust workspace as a separate step. Until then the Node-facing
  acceptance items are specified here and exercised through the worker role,
  which implements the ACP execution boundary and embeds `pi-acp`.

## Deploying it

There is no installer and no environment file. A deployment is one or two
`ExecStart` lines under whatever supervisor the operator already runs, and every
setting is a flag on that line. (`--join-code` additionally reads
`LOOM_JOIN_CODE`, because a secret is a poor thing to
put in argv; nothing else is read from the environment.)

The two supported shapes are **single box** (`loom server --local-worker`) and
**two machines** (`loom server` plus `loom worker --server-url …`). The container
form is [`../containers/docker-compose.yml`](../containers/docker-compose.yml),
described in [`containers.md`](containers.md).

Two machines — the control plane as its own unit:

```ini
# /etc/systemd/system/loom-server.service
[Unit]
Description=loom control plane
After=network-online.target
Wants=network-online.target

[Service]
ExecStart=/usr/local/bin/loom server --bind 127.0.0.1:38886 --data-dir /var/lib/loom/server
Restart=on-failure
User=loom
StateDirectory=loom

[Install]
WantedBy=multi-user.target
```

and the execution machine as its own unit:

```ini
# /etc/systemd/system/loom-worker.service
[Unit]
Description=loom execution worker
After=network-online.target
Wants=network-online.target

[Service]
ExecStart=/usr/local/bin/loom worker --server-url http://loom.example.com:38886 --name builder-1 --state /var/lib/loom/host-id
Restart=always
User=loom
StateDirectory=loom

[Install]
WantedBy=multi-user.target
```

The two units start the same installed file in a different role, and they have
separate resource domains and separate lifetimes. Stopping the worker cannot stop
the control plane, and vice versa.

Single box — both roles from one unit, with the server as the supervisor:

```ini
# /etc/systemd/system/loom-local.service
[Unit]
Description=loom control plane and local worker
After=network-online.target
Wants=network-online.target

[Service]
ExecStart=/usr/local/bin/loom server --bind 127.0.0.1:38886 --data-dir /var/lib/loom/server --local-worker
Restart=on-failure
User=loom
StateDirectory=loom

[Install]
WantedBy=multi-user.target
```

The single-box unit must be the laxer of the two — the worker child writes
workspaces and runs provider CLIs as the same user, so the server-only unit's
`ProtectSystem=strict` must not be copied onto it. The pair also shares one
process tree and one supervisor lifetime, which is the trade the single-box shape
makes (§ The decision).

Keep the bind on loopback. The API has no authentication and a worker executes
commands and reads files on its machine, so binding it to a public interface
(`--bind 0.0.0.0:38886`) exposes all of that. Put Tailscale or an authenticating
reverse proxy in front instead; see [`remote-access.md`](remote-access.md).
