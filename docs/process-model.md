# The process model: server, daemon, and the Node execution plane

This document is the boundary contract this repository commits to. It answers a
question the current code layout cannot answer on its own: **loom's control
plane is Rust, but bb's daemon — the part that actually runs provider CLIs — is
Node.** How do the two relate?

The two are **roles of one binary** (`loom`, built by `cargo build --release -p
loom`) and *two processes*. Which role a start takes is decided by the
invocation name — `loom server` / `loom daemon`, or the installed `loom-server`
/ `loom-daemon` symlinks onto the same file — and nothing else about the
boundary below changed with that: no lifetime, no resource domain and no
supervision relationship was merged.

## The decision

They are two processes with one protocol between them, and neither owns the
other.

- **`loom-server` (Rust)** is the control plane role: state, HTTP, WebSocket, the
  relay. It never starts, supervises, waits for, or requires a daemon.
- **`loom-daemon`** is the execution plane role: it runs on a machine, dials the
  server *outbound*, enrolls as a host, and executes work. It never starts,
  supervises, or requires the server to be co-located or in the same process
  tree.
- **The bb Node execution plane** (`apps/host-daemon`, checked in beside the
  Rust workspace in a later step) implements the *same* daemon contract as
  `loom-daemon`. `loom-daemon` is the reference implementation of that
  contract: the process shell, the enrollment/heartbeat lifecycle, and the test
  harness. When the Node daemon lands, it replaces the shell's body, not its
  boundary.

There is no FFI, no embedded Node runtime, and no shared cgroup. A daemon that
cannot reach a server is a daemon that reconnects; a server with no daemon is a
server that serves. That one-way dependency is what makes both deployment
shapes in `architecture.md` fall out for free.

### Why not run the Node daemon inside the Rust server

That is exactly bb's `runFullStack`: one process tree, one lifetime, one
resource domain. It is the cause of the two behaviours this fork is removing —
closing the execution plane takes the UI with it, and one machine exhausting
itself drags the control plane along. Keeping the boundary as a network
boundary is the point, not an implementation detail.

## Startup paths

Three, and the first two are the primitives; the third is a convenience.

| Path | Command | What it starts | What it assumes |
| --- | --- | --- | --- |
| **server-only** | `loom server` (installed as `loom-server`) | the control plane | nothing. No daemon, no data dir |
| **daemon-only** | `loom daemon --server-url <URL>` (installed as `loom-daemon`) | one execution machine | a reachable server URL |
| **full stack** | a supervisor starting both as *separate children* | both | a local server URL |

The server role is server-only by construction. It does not probe for a daemon,
and it does not exit when none connects — see the test
`a_server_with_no_daemon_is_up_and_a_remote_daemon_becomes_primary`. The
full-stack path is retained as "single-machine convenience", but it is a
supervisor over the same binary started twice: it starts two independent
children and can stop either alone.

## The daemon contract

A daemon uses the versioned internal WebSocket on `GET /internal/ws`; the
browser's bb-compatible realtime protocol remains exclusively on `GET /ws`:

```json
// daemon -> server (`/internal/ws`)
{"type":"enroll_host","name":"laptop"}
{"type":"enroll_host","host_id":"host_01M…","name":"laptop"}   // reconnect
{"type":"host_heartbeat","host_id":"host_01M…"}
{"type":"host_disconnect","host_id":"host_01M…"}
{"type":"run_report","report":{"host_id":"host_…","run_id":"run_…","thread_id":"thr_…","event":{…}}}
{"type":"replay","scope":{"kind":"host","id":"host_…"},"since":"01M…"}

// server -> daemon
{"type":"hello","protocol_version":3}
{"type":"host_enrolled","host":{"id":"host_01M…","status":"connected",…},"event_id":"01M…"}
{"type":"host_heartbeat_ack","host_id":"host_01M…","last_seen_at_ms":1}
{"type":"host_disconnected","host_id":"host_01M…"}
{"type":"run_report_ack","run_id":"run_…","accepted":true}
{"type":"replay_complete","scope":{…},"count":3}

// server -> daemon, through the relay (`host:{id}` scope)
{"type":"event","event_id":"01M…","scope":{"kind":"host","id":"host_…"},
 "payload":"<RunDispatch JSON as a string>","created_at_ms":1}
```

The dispatch and report shapes, and the terminal-state guarantee that goes with
them, are in [`provider-protocol.md`](provider-protocol.md).

Three properties the contract guarantees:

1. **Identity survives reconnects.** A daemon presents the `host_id` it was
   given; the server updates that host's status rather than minting a second
   machine. This is why a host is "a machine, not a connection".
2. **Closing the socket detaches the host.** A daemon that dies without a
   goodbye still produces `host_status_changed → disconnected`, so a UI can
   render the machine as gone while the server keeps serving.
3. **The server never dials the daemon.** Enrollment, heartbeat and dispatch
   all ride a connection the daemon opened, so daemons work behind NAT.

The HTTP surface mirrors the read side for non-socket callers:
`GET /api/v1/hosts`, `GET /api/v1/hosts/primary`,
`POST /api/v1/hosts/{id}/heartbeat`, `POST /api/v1/hosts/{id}/disconnect`, and
`POST /api/v1/hosts` (with an optional `id`) for enrollment.

## Primary host degradation

bb's server falls back to the *local* daemon's id file for its "primary host".
With no local daemon that fallback strands file browsing and host lookups on a
machine that is intentionally absent — the `host_unavailable` failure.

loom replaces the fallback with a policy, in `loom_domain::select_primary_host`:

1. the operator-declared local host (`LOOM_LOCAL_HOST_ID`), **only while a
   daemon is attached to it**;
2. otherwise the most recently seen connected host of any kind — the primary
   falls to a remote machine;
3. otherwise `None`, an explicit "no host enrolled yet".

The resolver cannot fail, so no route can turn "this machine has no daemon"
into an error. `GET /api/v1/hosts/primary` always answers `200`, with
`source: "local" | "remote" | "no_host"` so a client can render the degradation
honestly. A server-only deployment sets no local host at all and never enters
the local branch.

## Desktop shell supervision

The desktop shell is a UI plus, optionally, two process switches:

| Switch | Starts | Stopping it |
| --- | --- | --- |
| **local server** | a control-plane child (`loom server`, or the installed `loom-server` name) | the UI disconnects from that URL |
| **local execution daemon** | an execution-plane child (`loom daemon`, or `loom-daemon`) pointed at the current server URL | the host is marked disconnected; the window is untouched |

The two switches are independent. Turning the daemon off must not reload the
window, because a daemon is a property of the *machine* and the UI is a client
of a URL — the same reason a daemon on machine B can appear in a UI served by
machine A.

## What is in this repository today

- **`loom`** — the one artifact: a single binary (`cargo build --release -p
  loom`) that carries both roles, plus the `loom-server` / `loom-daemon`
  symlinks an install adds beside it. Which role a start takes comes from the
  invocation name, so `loom server` and `loom-server` are the same start, and so
  are `loom daemon` and `loom-daemon`. Nothing starts the other role.
- **The server role** — server-only control plane, with host enrollment,
  heartbeats, disconnects and primary-host resolution.
- **The daemon role** — the reference daemon-only entry point. It enrolls,
  follows its `host:{id}` room through the relay with replay on reconnect, and
  runs the provider the control plane dispatches.
- `loom-provider-protocol` — the dispatch/report contract between the two. ACP
  framing and provider-specific translation stay in the daemon, not in the
  control plane; the terminal-state guarantee is specified in
  [`provider-protocol.md`](provider-protocol.md).
- The bb Node sources (`apps/`, `packages/`) are **not yet checked in**; they
  land beside the Rust workspace as a separate step. Until then the Node-facing
  acceptance items are specified here and exercised through the daemon role,
  which implements the ACP execution boundary and embeds `pi-acp`.

## Deploying it

The ready-to-use units, environment templates and install script live in
[`../deploy/`](../deploy/README.md). The shape is:

Server-only, as its own unit:

```bash
# /etc/systemd/system/loom-server.service
[Service]
ExecStart=/usr/local/bin/loom server
Environment=LOOM_BIND=127.0.0.1:38886
Environment=LOOM_DATA_DIR=/var/lib/loom/server
```

Daemon-only, on a different machine, as its own unit (one instance per server):

```bash
# /etc/systemd/system/loom-host-daemon@builder-1.service
[Service]
ExecStart=/usr/local/bin/loom daemon
Environment=LOOM_SERVER_URL=https://loom.example.com
Environment=LOOM_HOST_NAME=builder-1
Environment=LOOM_DAEMON_STATE=/var/lib/loom/machines/builder-1/host-id
```

Both units start the same installed file in a different role — the
`/usr/local/bin/loom-server` and `/usr/local/bin/loom-daemon` names a unit may
use instead are symlinks onto it — and they have separate resource domains and
separate lifetimes. Stopping the daemon cannot stop the control plane, and vice
versa.

Keep `LOOM_BIND` on loopback. The API has no authentication and a daemon
executes commands and reads files on its machine, so binding it to a public
interface (bb's `--server-bind-host 0.0.0.0`, here `LOOM_BIND=0.0.0.0:38886`)
exposes all of that. Put Tailscale or an authenticating reverse proxy in front
instead; see [`remote-access.md`](remote-access.md).
