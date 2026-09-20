# Architecture

## Roles

Three roles, and only three.

| Role | Responsibility | Where it runs | How many |
| --- | --- | --- | --- |
| **UI** | Inspect and steer. Holds no state. | Browser, installed PWA, or the desktop shell's webview | any |
| **Server** | State, HTTP API, WebSocket, dispatch, serves the UI client it carries | one machine | 1 |
| **Worker** | Runs provider CLIs, provisions workspaces, executes tools | each execution machine | N |

A UI never talks to a worker. A worker never talks to a UI. Both only talk to
the server. This is the same shape as the reference design that motivated the
fork, and it is what makes the following true:

- opening the UI is "point a client at a URL" — nothing local is required;
- turning a machine into an execution machine is one enrollment, not a
  topology change;
- the server restarting, or a worker restarting, is not a UI event.

## The relay layer

The relay is split into two crates with a one-way dependency, because they have
different lifetimes and different failure modes.

```
                 ┌───────────────────────────────┐
   control plane │  loom, server role (Rust)     │
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
                   client WS ────┴──── worker WS
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

At runtime the server role wires the three layers together with one fixed reader
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

- **Backends are swappable.** Two are defined, and the trait is the only
  thing the relay sees:
  - [`backend::memory::MemoryBackend`] — in-process, zero configuration, lost
    on restart. The server's default.
  - [`backend::disk::DiskBackend`] — one crash-safe append-only file per
    shard under a data directory. Still in-process and dependency-free, but
    the replay window survives a restart.

  Nothing above `RelayBackend` changes between them.

  A third, [`backend::redis::RedisBackend`], put the log in Redis Streams so
  that *several servers* could share it. It was **removed**: sharing a log
  between servers needs a durable domain store with a single writer and a rule
  for which server owns an in-flight run, and loom has neither. Ship the shared
  log alone and a second server is a deployment that looks supported and is
  not. `--redis-url` and `LOOM_REDIS_URL` are tombstones that fail at startup
  with that reason.

  A backend whose IO is asynchronous reports failures at the call site;
  `DiskBackend` hands writes to a per-shard thread and therefore latches a
  failure instead, surfaced through `RelayBackend::backend_error` and reported
  by `/health` as `backend_error`. Reads keep working in that state, so the
  field is how an operator learns that durability — not availability — is what
  broke. The complementary case, a data directory that cannot be written at
  all, fails the backend at open rather than starting in a degraded mode.
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

### A reader resumes by event id, and the backend applies the cursor

A shard reader holds the last `EventId` it delivered and asks for "the next
`limit` records after this one". That cursor is a **backend parameter**
(`RelayBackend::read_after`), not a filter the caller applies to a bounded
read.

The distinction is not stylistic. A read bounded only by a timestamp and
limited *before* the cursor filter can return a full batch the caller must
discard entirely — and then return the same batch on the next pass, forever. A
burst of more than `limit` events minted in the same millisecond therefore
stalls that shard's reader permanently and silently swallows every later event
behind it. Expressing the cursor in the read makes progress unconditional:
the backend either returns records newer than the cursor or returns nothing
because there are none.

`crates/server/tests/pump.rs` pins this with a same-millisecond burst larger
than the batch limit.

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

### Two replay modes, and why they are not the same call

A client either has no cursor or it has one, and the two cases need opposite
ends of the window.

| Caller | Server returns | `has_more` |
| --- | --- | --- |
| No cursor | the **newest** `limit` frames (a tail view) | always `false` |
| Cursor | the **oldest** `limit` frames strictly after it | true when more exist |

A cursor replay is a **forward page**, and that is a correctness requirement,
not a preference. A resuming consumer advances its cursor to the last frame it
received. If the page held the *newest* frames, everything between the old
cursor and that page would be skipped, and the advanced cursor would leave the
gap permanently unrecoverable. Returning the oldest frames after the cursor
means repeating the call always moves forward and always converges: keep paging
while `has_more`.

This is the same failure mode as a reader that applies `limit` before its
cursor filter, and it is why both the resolver here and
`RelayBackend::read_after` take the cursor as input rather than leaving it to
the caller.

Regression coverage: `paging_from_a_cursor_recovers_every_missed_frame` over
every backend, and `a_reconnect_recovers_more_dispatches_than_one_replay_page`
end to end — a worker restarted with a cursor and a page limit of 4 still
executes all 10 queued runs.

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

### Domain state across restarts

The relay log says *what happened*; the control plane's `DomainRegistry` (and
the in-flight `RunRegistry`) is the *entity view* derived from it. With
`--data-dir` set, both the log and the entity view survive a restart. The
entity view is stored as a **snapshot plus a replay cursor**, not as a second
copy of the log:

- the snapshot holds projects, threads, hosts and environments, plus the
  newest `EventId` it incorporates and the runs that were in flight;
- recovery loads it and replays only the retained log events **after** that
  cursor;
- a thread left `working` by a restart is failed with a terminal run event, so
  "a thread cannot be stuck in `working`" holds across restarts too.

The design, its crash-safety argument and the choices that were *not* taken
(pure snapshot, pure replay, an external database) are in
[`domain-persistence.md`](domain-persistence.md).

### Backpressure

`Transport::send` returns `false` when a connection is closed or too far
behind. The hub counts that as a drop and does not buffer without bound. A
client that cannot keep up recovers by replaying from its last seen event id
rather than by making the server grow.

## What this replaces in bb

| bb behaviour | Cause | Here |
| --- | --- | --- |
| Multi-second event-loop stalls; `fetch failed` from local plugins | synchronous `better-sqlite3` on the single Node loop, an 11-index `events` table, unbounded truncation sweeps | storage behind an async seam; no sweep cursors |
| Worker restart kills in-flight agent turns | provider bridge transport is inherited stdio owned by the worker's parent | worker is an independent process; relay replays what it missed |
| An event the server refuses blocks every thread's events | one host-wide event queue spliced only on success | ordered per-scope log; a rejection cannot block another scope |
| UI freezes with the server | UI and server share one process/cgroup | UI is a URL client; server and worker are separate units |
| Provider output handling wedges a turn | provider-specific bridge assumes one private wire format | ACP framing/translation is isolated in the worker and the server reaps silent runs |

## Deployment shapes

The relay adds no infrastructure. Sharding, replay, dedup and the fixed reader
count are all present with the in-process backend.

### A. Single machine

```
desktop shell → loom server (loopback) → loom-relay (in-process) → UI + local worker
```

### B. Server plus execution machines

```
                  ┌────────────────────────────┐
                  │ loom server (systemd unit) │
                  │ loom-relay (in-process)    │
                  └──────────────┬─────────────┘
        ┌──────────────┬─────────┼──────────┬──────────────┐
        ▼              ▼         ▼          ▼              ▼
    worker 1       worker 2   worker 3   PWA (phone)   desktop (webview)
```

The server and each worker are separate services with separate data
directories and separate resource domains. One agent exhausting a machine
cannot take the control plane with it. Workers make outbound connections only,
so they work behind NAT. This shape is described in
[`process-model.md`](process-model.md) and packaged as the compose file in
[`containers.md`](containers.md), with the network boundary in
[`remote-access.md`](remote-access.md) and the update rules in
[`upgrades.md`](upgrades.md).

### C. Restart-transparent within one server

A server that must not lose its replay window on restart needs no second
process: the log lives in a data directory (`--data-dir`), one append-only file
per shard, and a restart replays it. Workers reconnect and resume from the event
id they last saw, so the window — not the connection — is what makes the
upgrade transparent.

```
                  ┌────────────────────────────────────┐
                  │ loom server (systemd unit)         │
                  │ loom-relay + DiskBackend           │
                  │ /var/lib/loom/server/shard-0 … 7   │
                  └───────────────┬────────────────────┘
                                  │  workers reconnect and replay
                  ┌───────────────┴────────────────────┐
                  ▼                                    ▼
              worker 1                              worker 2
```

```bash
loom server --data-dir /var/lib/loom/server
```

Only the [`RelayBackend`] implementation changes between A/B and C: `loom-relay`,
the fixed `SHARD_COUNT` readers, retention, dedup and every handler above it are
untouched. Running **two servers** over one log is not a supported shape — see
the removed `RedisBackend` above.

We deliberately keep the reference design's **fixed shards plus fixed
readers** model rather than per-scope subscriptions: `SHARD_COUNT` is still a
constant, the process still runs exactly one reader per shard, and the
`shard_for` FNV-1a hash is still the only routing function — so a non-Rust
node can compute the same shard.

## UI as a URL client

The UI is plain static assets served by the server, and it derives everything
it needs from its own origin. Pointing a client at a URL is the whole
configuration.

| Client | How it is configured |
| --- | --- |
| Browser | open the URL |
| Installed PWA (desktop, iOS, Android) | same, then "install" |
| Desktop shell | webview pointed at a URL; optionally supervises a local server and/or worker |

That makes these three genuinely the same client, which is why the native
mobile app is not maintained here.

This is implemented. The server role serves the UI from the same origin as the
API. The UI is the product app in `apps/app`, built with
`pnpm --filter @bb/app run build` and compiled into the binary by
`crates/server/build.rs`, so a server deployment is one artifact: no bundle path
to configure, and no way for a server's client to differ from its release.
The `--ui-proxy` flag reverse-proxies to a dev server instead, and is development
only. The client contract — typed `/api/v1` routes, the public `/ws`
subprotocol with bb targets answered by `changed`/`pong`, and a reconnect that
invalidates and reloads rather than replaying — is in [`ui.md`](ui.md).

### Optional local worker

The desktop shell may start, and independently stop, a local worker. A worker
is a property of a *machine*, not of a UI, so closing it must not affect the
UI. Two consequences drive the implementation:

1. Server and worker must be independently startable processes. bb couples
   them in one full-stack launcher, so this is a prerequisite for the feature,
   not a follow-up.
2. The server's notion of a "primary host" currently falls back to the local
   worker's id file. With no local worker, that fallback must not strand file
   browsing and host lookups on a host that is intentionally absent.

Both are now implemented: the server role is server-only, the worker role is the
independent execution-plane entry point, and primary-host resolution degrades
instead of failing. They are `loom server` and `loom worker` — one binary, two
roles, still two processes. A single box may also run both from one command,
`loom server --local-worker`, which starts and supervises one worker child
without merging the roles: still two processes, now one supervisor. The boundary
contract — the wire protocol, the primary policy, and the desktop shell's two
supervision switches — is specified in
[`process-model.md`](process-model.md).

## Open questions

- Whether the desktop shell earns its maintenance cost once the UI is a URL
  client, or whether an installed PWA covers it. Current recommendation: keep
  `apps/desktop` in the tree but reduce it to a webview pointed at a URL plus
  the two supervision switches, and delete nothing until that shell is proven
  (see [`ui.md`](ui.md) § "Desktop shell").
- How much of bb's existing Node host daemon is kept as-is: it is ~45k lines and its
  provider bridge is the part that actually touches agents.
