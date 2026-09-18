# loom

A hard fork of [bb](https://github.com/get-bb/bb) with a Rust control plane and a
fixed relay layer. No upstream tracking: this repository owns its code.

The goal is not "bb but rewritten". bb's instability is concentrated in a small
number of structural decisions, and each one is being replaced:

| bb | this fork |
| --- | --- |
| Single-threaded Node event loop doing synchronous SQLite work | Rust control plane; storage behind an async seam |
| Producers mutate an in-process WebSocket hub directly | Producers only publish to a scope; the relay owns delivery |
| Server and worker bound together, one cgroup, one lifetime | Server and worker are independent processes and deployment units |
| Plugin runtime executing arbitrary JS inside the server | No plugin runtime; providers are first-class |
| UI is whatever the local Electron shell boots | UI is a client of a URL; web, PWA and desktop are the same thing |

## Status

Early. The foundation is the relay layer, because every other decision depends
on it and it can be validated on its own.

- [x] `loom-relay` — scoped, sharded, replayable event log
- [x] `loom-relay-hub` — rooms, idempotent fan-out, backpressure signal
- [x] `loom` — one binary carrying both roles; `loom-server` (control plane: HTTP + WebSocket surface, publish reaches subscribers through the log) and `loom-worker` (execution plane) are roles of it, still two processes
- [x] `loom-domain` — projects, threads, hosts and environments as pure types and invariants
- [x] Managed projects: create / list / rename / archive / sources over HTTP, with threads and environments naming their project (`docs/projects.md`)
- [x] Server-only startup and an independently stoppable local worker: `loom server` and `loom worker`, or the installed symlinks that name the same file
- [x] `loom-provider-protocol` — the server↔worker ACP execution contract, replayable run events, and a terminal-state guarantee
- [x] The event model aligned with bb's `ThreadEvent` contract (35 provider event types) — see [`docs/event-model.md`](docs/event-model.md)
- [ ] Persist domain entities (the domain registry is in-process and lost on restart)
- [x] The server role hosts the UI from its own origin: the product app in `apps/app` is compiled into the binary (`crates/server/build.rs`), so the one artifact carries its client and nothing about serving it is configured (`docs/ui.md`)
- [x] The ported bb app (`apps/app`) is the only UI — same-origin typed `/api/v1` routes, the public `/ws` realtime contract, and machine-checked provenance against the pinned bb commit (`docs/ui-baseline.md`)
- [ ] Check in the Node execution plane (`apps/host-daemon`) against the worker contract
  (`loom worker` is the reference implementation of that contract and exercises
  all of it today)
- [x] Automations: domain, durable storage, typed HTTP surface, a cron/timezone scheduler and agent execution through the existing thread/run/ACP path (`docs/automations.md`)
- [x] Redis Streams relay backend for restart-transparent upgrades (`LOOM_REDIS_URL`)
- [x] bb's HTTP/WebSocket/worker contract exported to JSON Schema, with a Rust conformance harness (`docs/contract.md`)
- [x] CI on every push and PR: format, lint, the full test suite, the declared MSRV and contract reproducibility (`docs/ci.md`)

## Layout

```
crates/
  loom/         loom            the one binary, dispatching by invocation name
                                (`loom server` / `loom worker`) or by the
                                installed `loom-server` / `loom-worker` symlinks
  domain/       loom-domain     projects, threads, hosts, environments, scopes, events, runs
  relay/        loom-relay      scopes, event ids, retention, dedup, backends
  relay-hub/    loom-relay-hub  connections, rooms, delivery
  server/       loom-server     HTTP, WebSocket, protocol, dispatch, fixed readers, UI hosting
                                (the control-plane role's implementation)
  provider-protocol/  loom-provider-protocol  the server↔worker provider contract
  worker/       loom-worker     the execution plane's implementation: enrollment,
                                dispatch, ACP agents (the other role of `loom`)
  contract/     loom-contract   bb's exported contract as a conformance target
contracts/bb/                   generated JSON Schema from bb's contract packages
tools/contract-export/          the exporter that produces contracts/bb
apps/app/                       the product app: the only UI, compiled into
                                the binary by crates/server/build.rs
ui/packages/*                   the pinned bb packages the product app builds
                                against (domain, contract, thread-view, …)
        ui/provenance.json, ui/app-patch-ledger.json, ui/app-port-plan.json
                                the app's pin, per-file adaptation record and
                                route-level port plan, all machine-checked
deploy/         systemd units, environment templates, install/uninstall scripts,
                the container images and a compose example
docs/
  acp-adapter.md
  api-coverage.md
  architecture.md
  ci.md
  containers.md
  contract.md
  event-model.md
  domain-persistence.md
  handoff.md
  process-model.md
  projects.md
  provider-protocol.md
  provider-sessions-research.md
  provider-strategy.md
  redis-backend.md
  releasing.md
  ui.md
  ui-package-sync.md
  remote-access.md
  mobile.md
  upgrades.md
  deployment-verification.md
```

`apps/app` is the product app: bb's application source with loom's transport,
loom's routes and the unsupported surfaces removed, in the pnpm workspace and
built by the same `pnpm build` as everything else. It is the only UI this
repository serves. The baseline, dependency closure and product-surface
decisions are recorded in [`docs/ui-baseline.md`](docs/ui-baseline.md). The
manifest-driven machine-checkable registry, hashes and import list live in
[`ui/provenance.json`](ui/provenance.json), the per-file adaptation record in
[`ui/app-patch-ledger.json`](ui/app-patch-ledger.json), and the route-level port
plan in [`ui/app-port-plan.json`](ui/app-port-plan.json). The projection package
sync policy remains in [`docs/ui-package-sync.md`](docs/ui-package-sync.md).

## Build and test

The client is compiled into the server, so the app's bundle is a prerequisite of
every cargo command:

```bash
pnpm install
pnpm build            # ui/packages/*, then the product app → apps/app/dist
pnpm test
pnpm provenance:check
pnpm port-plan:check
pnpm check:bundle     # the app's boot and lazy-route budget, after the build

cargo build --release -p loom   # the one binary both roles run from
cargo test --workspace
cargo clippy --workspace --all-targets
cargo fmt --all
```

`cargo build --release -p loom` puts the artifact at `target/release/loom`;
`loom server` starts the control plane and `loom worker` an execution machine,
and `deploy/install.sh` adds the `loom-server` / `loom-worker` symlinks so
anything that spawns a binary by name keeps working. For a quick start,
`cargo run -p loom -- server`.

The ported `thread-view`, `client-core`, `core-ui`, `shared-ui`, and contract
packages live under `ui/packages/` and are what the app builds against. Their
source pin and deliberate hard-fork synchronization policy are recorded in
[`docs/ui-package-sync.md`](docs/ui-package-sync.md); the full app baseline and
migration boundary are in [`docs/ui-baseline.md`](docs/ui-baseline.md).

`pnpm build` produces the UI bundle at `apps/app/dist`, which `cargo build`
compiles into the binary (`crates/server/build.rs`); the server never reads a UI
directory at runtime, so there is no path to configure. That makes the build a
prerequisite rather than an optional extra: without `apps/app/dist`, `cargo
build` stops and names `pnpm --filter @bb/app run build` as the instruction. The
full contract is [`docs/ui.md`](docs/ui.md).

CI runs the check forms of these on every push and PR, plus the declared MSRV
and the contract-reproducibility check; [`docs/ci.md`](docs/ci.md) lists the
jobs, the required checks and the measured duration.

No external services are required: the default backend is in-process. A
`LOOM_DATA_DIR` keeps the replay window on local disk **and** persists the
domain entity view (projects, threads, hosts, environments) across restarts;
`LOOM_REDIS_URL` moves the log to Redis Streams so it is shared and survives a
server upgrade. See [`docs/domain-persistence.md`](docs/domain-persistence.md)
for how domain state recovers, and
[`docs/redis-backend.md`](docs/redis-backend.md) for the Redis deployment
contract.

Run it:

```bash
# Server-only: the control plane and nothing else. It never starts a worker
# and never exits because one is missing.
cargo run -p loom -- server       # listens on 127.0.0.1:38886

curl localhost:38886/health
curl localhost:38886/api/v1/hosts/primary
curl -X POST localhost:38886/api/v1/publish \
  -H 'content-type: application/json' \
  -d '{"scope":{"kind":"thread","id":"thr_1"},"payload":"{\"hello\":\"loom\"}"}'
curl 'localhost:38886/api/v1/replay?scope_kind=thread&scope_id=thr_1'
```

Worker-only, in a second terminal. It dials the server outbound and can stop
without touching it:

```bash
cargo run -p loom -- worker --server-url http://127.0.0.1:38886 --name laptop
# → loom-worker "laptop" enrolled as host_01M… with http://127.0.0.1:38886
```

The same binary runs both roles: `loom server` and `loom worker` are the
explicit form, and the installed `loom-server` / `loom-worker` symlinks are the
same file answering to the old names.

With no worker at all, `GET /api/v1/hosts/primary` answers `200` with
`{"host":null,"source":"no_host"}` rather than an error — a server-only
deployment degrades, it does not break. The full boundary contract is in
[`docs/process-model.md`](docs/process-model.md).

Minimal domain commands — create a thread, message it, register a host. Each
publishes a typed `loom-domain` event through the relay:

```bash
# Register a host; the `host_registered` event goes to host:{id}.
curl -X POST localhost:38886/api/v1/hosts \
  -H 'content-type: application/json' -d '{"name":"laptop"}'

# List projects. The server seeds one personal project on first start; it is
# an ordinary project from then on. A thread must name its project. The body is
# a bare array of bb's `projectSchema` — no envelope.
curl localhost:38886/api/v1/projects

# Create a thread; the `thread_created` event goes to the project's scope. The
# body is bb's `threads.create` shape and the response is the thread itself.
curl -X POST localhost:38886/api/v1/threads \
  -H 'content-type: application/json' \
  -d '{"projectId":"proj_...","origin":"app","input":[],"environment":{"type":"project-default"}}'

# Message a thread; it appends and, from idle, starts a run. Both events go
# to thread:{id}, in order. With a worker connected, a `RunDispatch` is also
# published to `host:{id}`; the provider's events (assistant/reasoning deltas,
# tool items, and the terminal `turn/completed`) come back as
# `thread_run_event`s on the thread scope, each carrying a bb `ThreadEvent`.
curl -X POST localhost:38886/api/v1/threads/thr_.../messages \
  -H 'content-type: application/json' -d '{"content":"hello"}'

# In-flight runs, if you want to see the dispatch table.
curl localhost:38886/api/v1/runs
```

Connect a raw relay/worker client on `ws://127.0.0.1:38886/internal/ws`, send
`{"type":"subscribe","scope":{"kind":"thread","id":"thr_1"}}`, and the
published frame arrives.

The UI is served from the same origin: open `http://127.0.0.1:38886/`. It is the
product app, built with `pnpm --filter @bb/app run build` and compiled into the
binary — there is no UI directory to point at and no UI variable to
set. `LOOM_UI_PROXY` is the one override, development only, and reverse-proxies
to a dev server; `LOOM_UI_DIR` is not read any more, so a line left over from the
shape that served a bundle from disk is inert. The client derives its server from
its own origin, talks
typed `/api/v1` routes and the public `/ws` protocol, and recovers from a
reconnect by invalidating and reloading — the contract is in
[`docs/ui.md`](docs/ui.md).

The provider contract — ACP dispatch through the relay, the report path and the
guarantee that a run always ends — is specified in
[`docs/provider-protocol.md`](docs/provider-protocol.md).

Deploying the multi-machine shape (server plus execution machines) is
[`deploy/`](deploy/README.md): the one binary plus its role symlinks, two
systemd units, environment templates, and an idempotent install/uninstall
script. The same two processes are published as container images — `docker run`,
or a `docker compose` all-in-one —
[`docs/containers.md`](docs/containers.md), which is also where the limits of a
containerised execution worker are written down. Remote access is
[`docs/remote-access.md`](docs/remote-access.md) (Tailscale Serve in front of a
loopback bind), phones are
[`docs/mobile.md`](docs/mobile.md) (installed PWA, no worker), and upgrades are
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
