# The process model: server, daemon, and the Node execution plane

This document is the boundary contract this repository commits to. It answers a
question the current code layout cannot answer on its own: **loom's control
plane is Rust, but bb's daemon — the part that actually runs provider CLIs — is
Node.** How do the two relate?

## The decision

They are two processes with one protocol between them, and neither owns the
other.

- **`loom-server` (Rust)** is the control plane: state, HTTP, WebSocket, the
  relay. It never starts, supervises, waits for, or requires a daemon.
- **`loom-daemon`** is the execution plane: it runs on a machine, dials the
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
| **server-only** | `loom-server` | the control plane | nothing. No daemon, no data dir |
| **daemon-only** | `loom-daemon --server-url <URL>` | one execution machine | a reachable server URL |
| **full stack** | a supervisor starting both as *separate children* | both | a local server URL |

`loom-server` is server-only by construction. It does not probe for a daemon,
and it does not exit when none connects — see the test
`a_server_with_no_daemon_is_up_and_a_remote_daemon_becomes_primary`. The
full-stack path is retained as "single-machine convenience", but it is a
supervisor over the same two binaries: it starts two independent children and
can stop either alone.

## The daemon contract

A daemon speaks the same WebSocket as a UI, on `GET /ws`:

```json
// daemon -> server
{"type":"enroll_host","name":"laptop"}
{"type":"enroll_host","host_id":"host_01M…","name":"laptop"}   // reconnect
{"type":"host_heartbeat","host_id":"host_01M…"}
{"type":"host_disconnect","host_id":"host_01M…"}

// server -> daemon
{"type":"welcome","connection_id":1,"protocol_version":1}
{"type":"host_enrolled","host":{"id":"host_01M…","status":"connected",…},"event_id":"01M…"}
{"type":"host_heartbeat_ack","host_id":"host_01M…","last_seen_at_ms":1}
{"type":"host_disconnected","host_id":"host_01M…"}
```

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
| **local server** | a `loom-server` child | the UI disconnects from that URL |
| **local execution daemon** | a `loom-daemon` child pointed at the current server URL | the host is marked disconnected; the window is untouched |

The two switches are independent. Turning the daemon off must not reload the
window, because a daemon is a property of the *machine* and the UI is a client
of a URL — the same reason a daemon on machine B can appear in a UI served by
machine A.

## What is in this repository today

- `loom-server` — server-only control plane, with host enrollment, heartbeats,
  disconnects and primary-host resolution.
- `loom-daemon` — the reference daemon-only entry point. It is the process
  shell; provider execution arrives with the Node execution plane.
- The bb Node sources (`apps/`, `packages/`) are **not yet checked in**; they
  land beside the Rust workspace as a separate step. Until then the Node-facing
  acceptance items are specified here and exercised through `loom-daemon`,
  which implements the identical wire contract.

## Deploying it

Server-only, as its own unit:

```bash
# /etc/systemd/system/loom-server.service
[Service]
ExecStart=/usr/local/bin/loom-server
Environment=LOOM_BIND=0.0.0.0:38886
Environment=LOOM_DATA_DIR=/var/lib/loom
```

Daemon-only, on a different machine, as its own unit:

```bash
# /etc/systemd/system/loom-daemon.service
[Service]
ExecStart=/usr/local/bin/loom-daemon --server-url https://loom.example.com
Environment=LOOM_HOST_NAME=builder-1
Environment=LOOM_DAEMON_STATE=/var/lib/loom/host-id
```

The two units have separate resource domains and separate lifetimes. Stopping
`loom-daemon` cannot stop `loom-server`, and vice versa.
