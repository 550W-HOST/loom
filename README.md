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
- [ ] Persist the log (durable backend) and port the real domain model
- [ ] Port the bb web UI unchanged, served by the Rust server
- [ ] Server-only startup and independently stoppable local daemon
- [ ] Redis/NATS relay backend for restart-transparent upgrades

## Layout

```
crates/
  relay/        loom-relay      scopes, event ids, retention, dedup, backends
  relay-hub/    loom-relay-hub  connections, rooms, delivery
  server/       loom-server     HTTP, WebSocket, protocol, fixed readers
docs/
  architecture.md
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
cargo run -p loom-server            # listens on 127.0.0.1:38886

curl localhost:38886/health
curl -X POST localhost:38886/api/v1/publish \
  -H 'content-type: application/json' \
  -d '{"scope":{"kind":"thread","id":"thr_1"},"payload":"{\"hello\":\"loom\"}"}'
curl 'localhost:38886/api/v1/replay?scope_kind=thread&scope_id=thr_1'
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
