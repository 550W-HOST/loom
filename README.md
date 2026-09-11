# loom

A hard fork of [bb](https://github.com/get-bb/bb) with a Rust control plane and a
fixed relay layer. No upstream tracking: this repository owns its code.

The goal is not "bb but rewritten". bb's instability is concentrated in a small
number of structural decisions, and each one is being replaced:

| bb | this fork |
| --- | --- |
| Single-threaded Node event loop doing synchronous SQLite work | Rust control plane; storage behind an async seam |
| Producers mutate an in-process WebSocket hub directly | Producers only publish to a scope; the relay owns delivery |
| Server and daemon bound together, one cgroup, one lifetime | Server and daemon are independent processes and deployment units |
| Plugin runtime executing arbitrary JS inside the server | No plugin runtime; providers are first-class |
| UI is whatever the local Electron shell boots | UI is a client of a URL; web, PWA and desktop are the same thing |

## Status

Early. The foundation is the relay layer, because every other decision depends
on it and it can be validated on its own.

- [x] `loom-relay` — scoped, sharded, replayable event log
- [x] `loom-relay-hub` — rooms, idempotent fan-out, backpressure signal
- [x] `loom-server` — HTTP + WebSocket surface; publish reaches subscribers through the log
- [x] `loom-domain` — projects, threads, hosts and environments as pure types and invariants
- [x] Server-only startup and an independently stoppable local daemon (`loom-daemon`)
- [ ] Persist domain entities (the domain registry is in-process and lost on restart)
- [ ] Port the bb web UI unchanged, served by the Rust server
- [ ] Check in the Node execution plane (`apps/host-daemon`) against the daemon contract
- [ ] Redis/NATS relay backend for restart-transparent upgrades

## Layout

```
crates/
  domain/       loom-domain     projects, threads, hosts, environments, scopes, events
  relay/        loom-relay      scopes, event ids, retention, dedup, backends
  relay-hub/    loom-relay-hub  connections, rooms, delivery
  server/       loom-server     HTTP, WebSocket, protocol, fixed readers
  daemon/       loom-daemon     the execution plane as an independent process
docs/
  architecture.md
  process-model.md
```

Application code from the bb fork (`apps/`, `packages/`, `plugins/`) lands here
next, alongside the Rust workspace rather than replacing it.

## Build and test

```bash
cargo test --workspace
cargo clippy --workspace --all-targets
cargo fmt --all
```

No external services are required: the default backend is in-process.

Run it:

```bash
# Server-only: the control plane and nothing else. It never starts a daemon
# and never exits because one is missing.
cargo run -p loom-server            # listens on 127.0.0.1:38886

curl localhost:38886/health
curl localhost:38886/api/v1/hosts/primary
curl -X POST localhost:38886/api/v1/publish \
  -H 'content-type: application/json' \
  -d '{"scope":{"kind":"thread","id":"thr_1"},"payload":"{\"hello\":\"loom\"}"}'
curl 'localhost:38886/api/v1/replay?scope_kind=thread&scope_id=thr_1'
```

Daemon-only, in a second terminal. It dials the server outbound and can stop
without touching it:

```bash
cargo run -p loom-daemon -- --server-url http://127.0.0.1:38886 --name laptop
# → loom-daemon "laptop" enrolled as host_01M… with http://127.0.0.1:38886
```

With no daemon at all, `GET /api/v1/hosts/primary` answers `200` with
`{"host":null,"source":"no_host"}` rather than an error — a server-only
deployment degrades, it does not break. The full boundary contract is in
[`docs/process-model.md`](docs/process-model.md).

Minimal domain commands — create a thread, message it, register a host. Each
publishes a typed `loom-domain` event through the relay:

```bash
# Register a host; the `host_registered` event goes to host:{id}.
curl -X POST localhost:38886/api/v1/hosts \
  -H 'content-type: application/json' -d '{"name":"laptop"}'

# Create a thread; the `thread_created` event goes to the project's scope.
curl -X POST localhost:38886/api/v1/threads \
  -H 'content-type: application/json' -d '{}'

# Message a thread; it appends and, from idle, starts a run. Both events go
# to thread:{id}, in order.
curl -X POST localhost:38886/api/v1/threads/thr_.../messages \
  -H 'content-type: application/json' -d '{"content":"hello"}'
```

Connect a client on `ws://127.0.0.1:38886/ws`, send
`{"type":"subscribe","scope":{"kind":"thread","id":"thr_1"}}`, and the
published frame arrives.

## The one idea worth reading first

The control plane never touches a connection. It calls:

```rust
relay.publish(Scope::Thread(thread_id), frame)?;
```

Everything else — which shard that lands on, how long it is retained, which
node replays it after an outage, which sockets on that node receive it, and how
a duplicate is suppressed — is the relay layer's problem. That is what makes
server restarts, reconnects and multi-machine dispatch tractable instead of
being tangled through every route handler.

See [`docs/architecture.md`](docs/architecture.md).

## License

MIT. The forked bb sources keep their original license and notices.
