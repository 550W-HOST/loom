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
- [x] `loom-provider-protocol` — the server↔daemon provider contract, plus a Pi bridge in `loom-daemon`: dispatch through the relay, replayable run events, and a terminal-state guarantee
- [ ] Persist domain entities (the domain registry is in-process and lost on restart)
- [x] `loom-server` hosts the UI from its own origin; a client subscribes to `thread:{id}` through the relay and reconnects by subscribe-then-replay (`docs/ui.md`)
- [ ] Check in the bb web UI (`apps/app`) and serve its built bundle unchanged via `LOOM_UI_DIR`
- [ ] Check in the Node execution plane (`apps/host-daemon`) against the daemon contract
  (`loom-daemon` is the reference implementation and exercises the whole contract today)
- [x] Redis Streams relay backend for restart-transparent upgrades (`LOOM_REDIS_URL`)

## Layout

```
crates/
  domain/       loom-domain     projects, threads, hosts, environments, scopes, events, runs
  relay/        loom-relay      scopes, event ids, retention, dedup, backends
  relay-hub/    loom-relay-hub  connections, rooms, delivery
  server/       loom-server     HTTP, WebSocket, protocol, dispatch, fixed readers, UI hosting
  provider-protocol/  loom-provider-protocol  the server↔daemon provider contract
  daemon/       loom-daemon     the execution plane: enrollment, dispatch, Pi bridge
ui/             the reference UI client: buildless, served by loom-server
deploy/         systemd units, environment templates, install/uninstall scripts
docs/
  architecture.md
  process-model.md
  provider-protocol.md
  redis-backend.md
  ui.md
  remote-access.md
  mobile.md
  upgrades.md
  deployment-verification.md
```

Application code from the bb fork (`apps/`, `packages/`, `plugins/`) lands here
next, alongside the Rust workspace rather than replacing it.

## Build and test

```bash
cargo test --workspace
cargo clippy --workspace --all-targets
cargo fmt --all
```

No external services are required: the default backend is in-process. A
`LOOM_DATA_DIR` keeps the replay window on local disk; `LOOM_REDIS_URL` moves
it to Redis Streams so it is shared and survives a server upgrade. See
[`docs/redis-backend.md`](docs/redis-backend.md) for the deployment contract.

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
# to thread:{id}, in order. With a daemon connected, a `RunDispatch` is also
# published to `host:{id}`; the provider's output, tool calls and terminal
# event come back as `thread_run_event`s on the thread scope.
curl -X POST localhost:38886/api/v1/threads/thr_.../messages \
  -H 'content-type: application/json' -d '{"content":"hello"}'

# In-flight runs, if you want to see the dispatch table.
curl localhost:38886/api/v1/runs
```

Connect a client on `ws://127.0.0.1:38886/ws`, send
`{"type":"subscribe","scope":{"kind":"thread","id":"thr_1"}}`, and the
published frame arrives.

The UI is served from the same origin: open `http://127.0.0.1:38886/`. With no
configuration that is the reference client compiled into the binary; point
`LOOM_UI_DIR` at a built bundle (the ported bb UI) or `LOOM_UI_PROXY` at a
frontend dev server. The client derives its server from its own origin and
reconnects with subscribe-then-replay — the contract is in
[`docs/ui.md`](docs/ui.md).

The provider contract — dispatch through the relay, the report path, the
stdout guard and the guarantee that a run always ends — is specified in
[`docs/provider-protocol.md`](docs/provider-protocol.md).

Deploying the multi-machine shape (server plus execution machines) is
[`deploy/`](deploy/README.md): two systemd units, environment templates, and an
idempotent install/uninstall script. Remote access is
[`docs/remote-access.md`](docs/remote-access.md) (Tailscale Serve in front of a
loopback bind), phones are
[`docs/mobile.md`](docs/mobile.md) (installed PWA, no daemon), and upgrades are
[`docs/upgrades.md`](docs/upgrades.md). The recorded clean-machine run is
[`docs/deployment-verification.md`](docs/deployment-verification.md).

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
