# Shared Redis backend

The relay can keep its log in Redis Streams instead of in process memory or in
local files. This is the backend for one specific requirement:

> **restarting or upgrading `loom-server` must not disconnect connected
> daemons, and a second server node must be able to attach to the same window.**

Everything else — replay, idempotence, per-scope ordering, decoupled producers
and consumers — the in-process backend already provides. This page is the
deployment contract for the case where the log must outlive the server process
*and* be shared.

The problem this solves is the one bb's inherited stdio transport caused
(bb #3143): when a daemon's transport is owned by the process being replaced,
every self-update kills the in-flight work. Here the server becomes stateless
with respect to delivery: it appends to and reads from a log that a peer
process owns.

## Enabling it

```bash
LOOM_REDIS_URL=redis://127.0.0.1:6379 loom-server
```

`LOOM_DATA_DIR` (local disk) and `LOOM_REDIS_URL` (shared) are alternatives;
setting both is a startup error rather than a silent preference. With neither,
the server uses the in-process backend and needs no configuration at all.

The URL form is `redis://[user][:password]@host[:port][/db]`:

```bash
redis://cache.internal:6379
redis://:s3cret@cache.internal:6379/2
redis://loom:p%40ss@cache.internal:6379
redis://[::1]:6379/1
```

Query parameters are ignored. There is no `rediss://`: see TLS below.

## What it stores

One Redis Stream per relay shard, `<prefix>:shard:0` … `<prefix>:shard:7`,
where `<prefix>` defaults to `loom:relay`. A record is one stream entry:

| field | meaning |
| --- | --- |
| `e` | the `EventId`, 26 Crockford base32 characters |
| `t` | `created_at_ms`, the producer timestamp |
| `o` | the producing node |
| `k` | the scope kind (`thread`, `host`, …) |
| `i` | the scope id |
| `p` | the payload bytes, verbatim |

The Redis stream id is Redis's own and is never used for routing. Routing is
still `shard_for(kind, id)` — a hand-rolled FNV-1a over `kind\0id` mod
`SHARD_COUNT`, deliberately not `std::hash::DefaultHasher`, so every node and
any non-Rust implementation computes the same shard.

Two prefixes on one Redis are two independent relay logs. Two *deployments*
sharing a Redis must not share a prefix, or they will merge their logs.

## Deployment requirements

- **Redis 6.0 or newer.** 5.0 works if no ACL user is configured; `AUTH
  <user> <password>` needs 6.0. RESP2 only.
- **A single primary, reachable over plain TCP.** The client does not follow
  `MOVED`/`ASK` redirects and does not speak Sentinel. For HA, run a proxy or a
  managed endpoint that always presents the primary on one address.
- **No TLS in the client.** Terminate TLS in front of Redis (stunnel, Envoy,
  HAProxy, a service mesh sidecar, or a managed TLS endpoint) and point
  `LOOM_REDIS_URL` at `redis://` of the terminator. This is deliberate: a TLS
  stack is a large dependency for a layer that otherwise has none.
- **Persistence, if you want Redis's own restarts to be transparent.**
  `appendonly yes` (AOF) is the right default; `appendfsync everysec` is a
  reasonable balance. With AOF off, an RDB snapshot interval can lose the tail.
  Note the distinction: server-restart transparency only needs Redis to stay
  up; Redis-restart transparency additionally needs its persistence.
- **A dedicated database or prefix.** Multi-tenancy is by `key_prefix` /
  `SELECT` index, not by ACLs.
- **Capacity.** Per shard the stream holds at most `backend_max_len` records
  (default 2000), enforced by `XADD … MAXLEN = <n>`, which is exact so the cap
  is a real bound. A shard's footprint is roughly `max_len × frame_size`; size
  Redis memory as `8 × max_len × frame_size` plus headroom, and set an
  `maxmemory` policy that does **not** evict relay keys (`noeviction`, or
  `volatile-*` with no TTL) — eviction would silently punch holes in replay.
- **Access control.** Bind Redis to a private interface, require a password,
  and firewall the port to server nodes. The log contains every client frame.

## Operations

- **Retention is unchanged.** `replay_grace` / `trim_horizon` / `ttl` and the
  invariant `trim_horizon > replay_grace` still hold; maintenance calls `trim`,
  which deletes entries by `created_at_ms` (a scan plus batched `XDEL`), so
  trimming can never eat the replay window. `replay_grace` and `trim_horizon`
  are process settings, so every node should be configured with the same
  retention or at least the same `replay_grace`.
- **Cost.** Every operation is one Redis round trip on a per-shard connection.
  `RelayBackend` is synchronous, so this backend blocks its caller for the
  round trip; keep Redis on the same host or a low-latency LAN. The connect and
  I/O timeouts bound how long a stalled Redis can hold a caller, and a mutation
  is never reported as durable before Redis acknowledges it.
- **Reconnection.** Connections are per shard and lazily opened. If an
  operation fails on IO, the connection is discarded, reopened once, and the
  operation retried; a Redis restart, a reset, or an idle-timeout kill is
  therefore transparent. A failure that repeats surfaces as a backend error.
  Writes go only to Redis — there is no local write-back cache to drift.
- **Outage behaviour.** While Redis is unreachable, publishes fail rather than
  buffer in memory. The default local backends are the right answer for a
  single machine that cannot tolerate that; the shared backend trades
  availability of *writes* for availability of the *window across nodes*.
- **Upgrading a server.** Stop the node, start the new binary with the same
  `LOOM_REDIS_URL`, and its readers replay from the shared window. Connected
  daemons have their own last-seen `EventId` and deduplicate the overlap, so a
  replayed frame is delivered once.
- **Resetting.** `RedisBackend::purge` deletes exactly the eight shard keys;
  `DEL <prefix>:shard:*` does the same from `redis-cli`.

## Why Redis Streams, not NATS

Both fit. Redis was chosen because it maps onto `RelayBackend` without an
adapter:

- `XADD` is `append`; `XRANGE` is `read`; `XDEL` is `trim`; `XLEN` is `len`.
  There is no consumer-group ack model to reconcile with the trait, because the
  relay's consumers are the fixed shard readers, not per-message consumers.
- `MAXLEN` gives the per-shard cap for free, which the retention policy
  already needs.
- It is the service more self-hosters already run, so "restart-transparent
  upgrades" does not require introducing new infrastructure.

NATS JetStream would be the choice if the constraint were many small clusters
with per-subject fan-out or a need for its richer retention/stream
configuration; it costs a new server and a new mental model for the same
result. The backend trait keeps that door open — a `NatsBackend` changes
nothing above `RelayBackend`.

## Testing

The relay's contract suite runs every scenario over the memory, disk and Redis
backends. The Redis cases are skipped unless `LOOM_REDIS_URL` names a reachable
server, so a default `cargo test` still needs no service:

```bash
docker run -d --rm --name loom-test-redis -p 127.0.0.1:6379:6379 redis:7.4
LOOM_REDIS_URL=redis://127.0.0.1:6379 cargo test -p loom-relay
```

`tests/redis.rs` adds the shared-only properties: a fresh backend replays what
a dead one wrote, two nodes append to one ordered log, a replayed frame is
deduplicated by `EventId`, and `purge` drops the window.
