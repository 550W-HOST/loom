# Architecture

## Roles

Three roles, and only three.

| Role | Responsibility | Where it runs | How many |
| --- | --- | --- | --- |
| **UI** | Inspect and steer. Holds no state. | Browser, installed PWA, or the desktop shell's webview | any |
| **Server** | State, HTTP API, WebSocket, dispatch, serves the UI bundle | one machine | 1 |
| **Daemon** | Runs provider CLIs, provisions workspaces, executes tools | each execution machine | N |

A UI never talks to a daemon. A daemon never talks to a UI. Both only talk to
the server. This is the same shape as the reference design that motivated the
fork, and it is what makes the following true:

- opening the UI is "point a client at a URL" — nothing local is required;
- turning a machine into an execution machine is one enrollment, not a
  topology change;
- the server restarting, or a daemon restarting, is not a UI event.

## The relay layer

The relay is split into two crates with a one-way dependency, because they have
different lifetimes and different failure modes.

```
                 ┌───────────────────────────────┐
   control plane │  loom-server (Rust)           │
   publishes ───▶│  relay.publish(scope, frame)   │
                 └───────────────┬───────────────┘
                                 │  Envelope { event_id, scope, payload, ... }
                 ┌───────────────▼───────────────┐
                 │  loom-relay                   │
                 │  sharding · retention · replay │
                 │  dedup · backends              │
                 │  (durable, replayable)         │
                 └───────────────┬───────────────┘
                                 │  Envelope stream
                 ┌───────────────▼───────────────┐
                 │  loom-relay-hub               │
                 │  rooms · subscriptions ·       │
                 │  per-connection dedup ·        │
                 │  backpressure                  │
                 │  (ephemeral, per-node)         │
                 └───────────────┬───────────────┘
                                 │
                   client WS ────┴──── daemon WS
```

`loom-relay` answers *does this event reach this node?* `loom-relay-hub` answers
*which sockets on this node get it, exactly once?*

### What is stored

The bytes appended to the log are the **client-facing frame**, not a bare
domain payload:

```json
{"type":"event","event_id":"01M27Y...","scope":{"kind":"thread","id":"thr_1"},"payload":"{\"hello\":\"loom\"}","created_at_ms":1789120438372}
```

The frame is built by the producer, once, before the append
(`protocol::build_event_frame`). Two properties follow, and both are
load-bearing:

- **Replay and live delivery are byte-identical.** A reconnecting client merges
  backlog and live frames by event id, with no shape translation.
- **The hub stays protocol-agnostic.** It forwards opaque bytes and never needs
  to know that an event has an id.

The event id therefore appears both in the envelope and inside the frame. That
duplication is deliberate: the envelope needs the id for ordering, trimming and
dedup; the frame needs it as the client's resume cursor.

At runtime `loom-server` wires the three layers together with one fixed reader
task per shard:

```text
  HTTP handler ──▶ Relay::publish_with(scope, build_frame)
                       │  append to one shard
  fixed readers  ──────┘  SHARD_COUNT tasks, one cursor each
                       │
                       ▼
                 Hub actor ──▶ subscriber sockets
```

A producer calls `AppState::publish` and nothing else. It never consults a
subscriber, a room or a socket.

### Why the split matters

- **Backends are swappable.** Three are defined, and the trait is the only
  thing the relay sees:
  - [`backend::memory::MemoryBackend`] — in-process, zero configuration, lost
    on restart. The server's default.
  - [`backend::disk::DiskBackend`] — one crash-safe append-only file per
    shard under a data directory. Still in-process and dependency-free, but
    the replay window survives a restart.
  - Redis/NATS — for a shared relay across nodes. Added only when a restart
    must be transparent to daemons *and* more than one node serves the log.

  Nothing above `RelayBackend` changes between them.
- **The relay is testable without sockets,** and the hub is testable without a
  broker.
- **The dependency direction is enforced.** `loom-relay` does not know
  `loom-relay-hub` exists. Connection state can never leak into routing.

### Fixed fan-in

Events route into a constant number of shards:

```rust
pub const SHARD_COUNT: u8 = 8;
pub fn shard_for(kind: &str, id: &str) -> ShardId   // FNV-1a over "kind\0id", mod 8
```

Every node runs one reader per shard. Blocked reader count is therefore
`node_count * SHARD_COUNT` and does not grow with the number of live threads,
projects or hosts. A thousand concurrent conversations cost the same fan-in as
one.

The hash is hand-rolled FNV-1a rather than `std::hash::DefaultHasher` on
purpose: the same value is computed by every node and by any future non-Rust
backend, so it must not be allowed to change between Rust releases.

### Identity and replay

Every event carries an `EventId` — 48-bit millisecond timestamp plus 80 bits of
entropy, rendered as 26 Crockford base32 characters, monotonic within a
process. Two consequences:

- replay is "everything at or after X", not a separate sequence allocation;
- delivery can be attempted more than once.

Because delivery can be attempted more than once, every consumer deduplicates
by id. That is what makes replay and the local fast path coexist: a frame that
arrives twice is delivered once, and a frame that arrives late is still
delivered.

### Retention

```
  now ─────────────────────────────────────────────▶ time
      │<── replay_grace ──>│<── trim_horizon ──>│
      │  guaranteed replay │  retained, may be  │
      │  for every reader  │  trimmed at will   │
      │                    │                    │
      └──────── ttl ───────┴────────────────────┘
```

`trim_horizon` must strictly exceed `replay_grace`, so trimming can never eat
the window a recovering reader depends on. The relationships are validated, not
assumed: getting them backwards breaks replay exactly after an outage, which is
the worst possible moment to discover it.

### Backpressure

`Transport::send` returns `false` when a connection is closed or too far
behind. The hub counts that as a drop and does not buffer without bound. A
client that cannot keep up recovers by replaying from its last seen event id
rather than by making the server grow.

## What this replaces in bb

| bb behaviour | Cause | Here |
| --- | --- | --- |
| Multi-second event-loop stalls; `fetch failed` from local plugins | synchronous `better-sqlite3` on the single Node loop, an 11-index `events` table, unbounded truncation sweeps | storage behind an async seam; no sweep cursors |
| Daemon restart kills in-flight agent turns | provider bridge transport is inherited stdio owned by the daemon's parent | daemon is an independent process; relay replays what it missed |
| An event the server refuses blocks every thread's events | one host-wide event queue spliced only on success | ordered per-scope log; a rejection cannot block another scope |
| UI freezes with the server | UI and server share one process/cgroup | UI is a URL client; server and daemon are separate units |
| Provider stdout pollution wedges a turn | bridge expects stdout to be JSON-RPC with no guard | provider supervision is first-class, not a plugin |

## Deployment shapes

The relay adds no infrastructure. Sharding, replay, dedup and the fixed reader
count are all present with the in-process backend.

### A. Single machine

```
desktop shell → loom-server (loopback) → loom-relay (in-process) → UI + local daemon
```

### B. Server plus execution machines

```
                  ┌────────────────────────────┐
                  │ loom-server (systemd unit)    │
                  │ loom-relay (in-process)       │
                  └──────────────┬─────────────┘
        ┌──────────────┬─────────┼──────────┬──────────────┐
        ▼              ▼         ▼          ▼              ▼
    daemon 1       daemon 2   daemon 3   PWA (phone)   desktop (webview)
```

The server and each daemon are separate services with separate data
directories and separate resource domains. One agent exhausting a machine
cannot take the control plane with it. Daemons make outbound connections only,
so they work behind NAT.

### C. Shared relay, restart-transparent

For a single server that must not lose its replay window on restart, the
`DiskBackend` already covers it with no new process: the log lives in a data
directory (`LOOM_DATA_DIR`), one append-only file per shard. When a server
upgrade must additionally not disconnect running daemons *and* a second node
must attach to the same log, the backend moves to Redis Streams or NATS. Only
the `RelayBackend` implementation changes.

## UI as a URL client

The UI is plain static assets served by the server, and it derives everything
it needs from its own origin. Pointing a client at a URL is the whole
configuration.

| Client | How it is configured |
| --- | --- |
| Browser | open the URL |
| Installed PWA (desktop, iOS, Android) | same, then "install" |
| Desktop shell | webview pointed at a URL; optionally supervises a local server and/or daemon |

That makes these three genuinely the same client, which is why the native
mobile app is not maintained here.

### Optional local daemon

The desktop shell may start, and independently stop, a local daemon. A daemon
is a property of a *machine*, not of a UI, so closing it must not affect the
UI. Two consequences drive the implementation:

1. Server and daemon must be independently startable processes. bb couples
   them in one full-stack launcher, so this is a prerequisite for the feature,
   not a follow-up.
2. The server's notion of a "primary host" currently falls back to the local
   daemon's id file. With no local daemon, that fallback must not strand file
   browsing and host lookups on a host that is intentionally absent.

## Open questions

- Whether the desktop shell earns its maintenance cost once the UI is a URL
  client, or whether an installed PWA covers it.
- Which relay backend, if any, a single self-hosted server ever needs.
- How much of bb's existing Node daemon is kept as-is: it is ~45k lines and its
  provider bridge is the part that actually touches agents.
