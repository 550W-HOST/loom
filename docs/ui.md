# UI as a URL client

The UI holds no state and owns no server address. It is a set of static assets
served by `loom-server` from the same origin as the API, so the only thing a
client needs is the URL:

```
UI (browser / PWA / desktop webview) ── HTTP + WS ──▶ loom server ── relay ──▶ frames
                                                          ▲
                                                       worker (separate process)
```

A UI never talks to a worker, and a worker never talks to a UI. Pointing a
client at a URL is the whole configuration.

The UI is the product app in [`../apps/app`](../apps/app), built to a static
bundle and compiled into `loom-server`. It is the only client in this repository:
the buildless `ui/` reference client that used to be served by default is gone,
and what remains under `ui/` is `ui/packages/*` — the pinned bb packages the app
builds against ([`ui-baseline.md`](ui-baseline.md)).

## Hosting

`loom-server` serves the client from bytes inside its own binary:
`crates/server/build.rs` walks `apps/app/dist` at build time and generates the
table `crates/server/src/ui.rs` serves. Installing the server therefore installs
the UI. There is one artifact, one thing to upgrade, no bundle path to
configure, and no way to run a server whose client differs from its release.

Because the bundle is a **build input**, `pnpm --filter @bb/app run build` is a
prerequisite of any `cargo build`, `cargo test` or `cargo clippy` on a source
checkout: with `apps/app/dist` missing, the build stops and names that command.
The release pipeline and CI run it before the Rust jobs for the same reason
([`releasing.md`](releasing.md), [`ci.md`](ci.md)).

| Source | Selected by | Use |
| --- | --- | --- |
| Embedded product app | nothing — always | Production |
| Dev-server proxy | `--ui-proxy http://127.0.0.1:5173` | Frontend development with hot reload against the real server |

The proxy is an override rather than the other half of a choice: nothing else
replaces the embedded bundle, and the override is development only. `LOOM_UI_DIR`
is **no longer read**: it is inert, and the server always serves the client it
was built with — a leftover line in an environment file is not worth refusing to
boot over.

Rules that apply to both:

* A request for a **client route** (no file extension) that has no matching file
  falls back to `index.html`, so history routing works.
* A request for an **asset** (has an extension) that does not exist returns
  `404`. A missing script must not silently become an HTML page.
* A **non-GET/HEAD** fallback request returns `404` (the proxy forwards
  methods as-is, for the dev server).
* `/api/**`, `/ws` and `/internal/ws` never fall through to the UI. A mistyped API or
  socket route is a `404`, not the SPA shell with a `200`.
* Directory paths are never normalised silently: `..`, absolute paths and
  prefixes are rejected instead of clamped, so a bug cannot become a sandbox
  escape.

The proxy tunnels WebSocket upgrades as well as HTTP, so Vite's HMR socket keeps
working while `/api`, `/ws` and `/internal/ws` stay on `loom-server`. During
development the UI therefore still uses one origin and needs no CORS
configuration. It makes the server depend on a second process being up, so no
deployment uses it.

## The client contract

The client is the product app. It holds no server address:
`apps/app/src/lib/loom-http.ts` derives every request from
`window.location.origin`, and `apps/app/src/lib/ws.ts` builds the socket the same
way, so one origin is the whole connection story.

**HTTP.** Every route the app may call is a row in
`apps/app/src/lib/loom-api-routes.ts`, and every row lives under `/api/v1`. The
table decides the method — a caller cannot send a body with a `GET`, and a method
the contract does not declare is refused rather than issued — and path
parameters are encoded exactly once, with `.`/`..` segments rejected before a
URL is built (`loom-http.ts`).

**Realtime.** The socket is the public `/ws` on the same origin, negotiated with
the explicit `loom-bb-realtime-v1` subprotocol (`BB_REALTIME_SUBPROTOCOL` in
`ui/packages/domain/src/change-kinds.ts`). The app sends `subscribe` and
`unsubscribe` for a bb target, plus `ping`. The server's whole answer vocabulary
is `Changed` and `Pong` (`ServerMessage` in `crates/server/src/protocol.rs`),
and only messages matching a subscribed target are sent to the socket
(`crates/server/src/ws.rs`). The targets the app subscribes to are the list
targets (`thread-list`, `project-list`, `environment-list`, `host-list`),
`system`, and the detail targets (`thread-detail`, `project-detail`,
`environment-detail`) — all from
`apps/app/src/hooks/useRealtimeSubscription.ts`.

**A `changed` frame is an invalidation, not data.** It names an entity
(`thread`, `project`, `environment`, `host`, `system`), an optional id and the
change kinds that moved (`ui/packages/domain/src/change-kinds.ts`); the app
invalidates the queries those kinds cover and refetches them over HTTP
(`apps/app/src/hooks/realtime-cache-effects.ts`). A missed frame therefore costs
a stale row until the next change, never a corrupt timeline, and there is no
client-side replay cursor to keep.

**Reconnect is a cache event.** The manager pings every 25 s while the document
is visible, skipping the ping when the server wrote recently, and arms a 5 s
`pong` timer when it sends one (`REALTIME_PING_INTERVAL_MS` and
`REALTIME_PONG_TIMEOUT_MS` in `apps/app/src/lib/ws.ts`); a socket whose `pong`
does not arrive is replaced. After every (re)connect it re-subscribes every
active target and invalidates the queries whose data predates the disconnect
(`invalidateRealtimeQueriesAfterServerReconnect` in
`apps/app/src/hooks/cache-owners/system-cache-effects.ts`), which reloads them
over HTTP. That is the whole recovery path: the app never renders a frame as
data, so it needs no replay window pushed into it.

The server makes the other half of the same choice. When a connection falls
behind the relay broadcast, or the relay itself resets, it closes the socket
rather than sending a frame it cannot vouch for — the public protocol has no
replay cursor, so reconnecting and reloading *is* the recovery
(`crates/server/src/ws.rs`).

Product surfaces loom added on top of bb's routes ride the same vocabulary:
automations, for instance, are invalidated as `project:changed` for the project
that owns them, because the pinned client's targets and event names are fixed
(`docs/automations.md` § Invalidation).

### The internal relay, for debugging

`/internal/ws` is the relay's own socket, the one a worker and the relay speak —
not a client protocol, and the app never opens it. It is what to reach for when
a frame is missing: connect to `/internal/ws`, send
`{"type":"subscribe","scope":{"kind":"thread","id":"<id>"}}` for the same scope a
producer published to (no handler is involved), then fetch the backlog with
`GET /api/v1/replay?scope_kind=thread&scope_id=<id>&since=<last_event_id>`.

The order is load-bearing: a frame published between the two calls arrives live
*and* is in the window, and the client drops the duplicate by `event_id`, while
fetching the backlog first would miss whatever was published in the gap. Replay
and live frames are the same bytes (`docs/architecture.md` § "What is stored"),
so no shape translation is needed, and event ids are fixed-width monotonic ULIDs,
so sorting by id restores chronological order when the two streams interleave.

A payload there is a serialized `loom-domain` `DomainEvent`: dispatch on its
`type` tag. A thread view builds rows from `thread_message_added` and
`thread_run_event`; `thread_status_changed` is state rather than content, so it
updates the thread's status and adds no row (`timeline_row_for_event` in
`crates/server/src/http.rs`); a list view also renders `thread_updated`, which
carries a thread's fields after a rename, a re-file, a
visibility change or a tabs write (`crates/domain/src/event.rs`).
`thread_run_event` carries a bb `ThreadEvent` in its `event` field — the
contract the projection layer dispatches on. See
[`event-model.md`](event-model.md).

## Building the bundle

The app is an ordinary pnpm workspace project, and its build output is a build
input of the server: `crates/server/build.rs` embeds `apps/app/dist`, and nothing
in that directory is read, rewritten or templated at runtime.

```bash
pnpm install --frozen-lockfile
pnpm --filter @bb/app run build          # → apps/app/dist, the input to cargo build
cargo run -p loom -- server
```

`apps/app/dist` holds `index.html`, `assets/**` with content-hashed names, and
the PWA files (the manifest and its icon set). Every URL in it is absolute from
the root, which is why the server can serve the bundle at `/` and stop thinking
about it; content-hashed assets are immutable, everything else carries a short
`max-age`, and `index.html` is `no-cache`, so a new binary is picked up without a
hard refresh.

`pnpm --filter @bb/app run check:bundle` applies the committed boot and lazy-route
budget (`apps/app/bundle-budget.json`) to the build's own `bundle-stats.json`, so
a heavy dependency that creeps back onto the boot path fails a check rather than
a phone.

## Acceptance

The client is the one part of this repository whose defects are only visible in a
browser, so it has a suite that runs it as one: [`e2e/`](../e2e) is a pnpm
workspace package whose Playwright specs start `loom server` and `loom worker`
from `target/debug`, point the worker at an ACP stub, and drive the app in a
desktop and a phone viewport.

It is the acceptance checklist, executable: the shell renders from the server's
own origin, an unreachable server is reported rather than rendered as an empty
app, a thread is created from the composer, answers, and survives a reload, a
permission request blocks the turn until it is answered (allow *and* deny, with
the decision asserted at the agent), automations list, run and report, and a
machine reports going offline and recovers when its worker returns.

```bash
# from a checkout, with apps/app/dist built (see "Building the bundle")
cargo build -p loom
pnpm --filter @loom/e2e exec playwright install chromium
pnpm --filter @loom/e2e test
```

Two viewports because the product is one client on a desktop and a phone: on a
phone the sidebar is a drawer, and the suite opens it before asserting what is
inside. `e2e/helpers/stack.ts` documents what the stack is and how a test takes
the machine away and gives it back; [`ci.md`](ci.md#the-browser-acceptance-job)
has the job that runs it on every push.

## The app, and what it adapted

`apps/app` is bb's application source carrying loom's transport, loom's routes
and the unsupported surfaces removed, checked in as a ledger-controlled source
port. It is a normal workspace project, `pnpm --filter @bb/app run build` builds
it, and it is the only UI served.

The pin, the closure and the boundary are machine-checked rather than
remembered:

* [`../ui/provenance.json`](../ui/provenance.json) records the upstream commit,
  the app's complete file set with per-file bytes/SHA-256, the packages it
  builds against and the contract manifest's hashes.
  `node scripts/check-ui-provenance.mjs` recomputes all of it — against `BB_SRC`
  for the upstream side — and fails on unregistered, duplicate, overlapping,
  glob, hash, add/delete/rename or mode drift, including a symlink where a plain
  directory was expected.
* [`../ui/app-patch-ledger.json`](../ui/app-patch-ledger.json) is the per-file
  adaptation record: each local change is one entry with an issue, an owner, a
  reason and both hashes. Nothing is adapted off the books, and a package that is
  already here may not be re-vendored from npm instead.
* [`../ui/app-port-plan.json`](../ui/app-port-plan.json) is the route-level port
  plan, checked by `node scripts/analyze-ui-port.mjs`.

`ui/packages/*` holds the bb packages the app builds against: `@bb/domain`,
`@bb/server-contract`, `@bb/thread-view` (the event-to-timeline projection),
`@bb/client-core`, `@bb/core-ui`, `@bb/shared-ui`, `@bb/config`,
`@bb/sdk`, `@bb/desktop-contract`, `@bb/host-daemon-contract`,
`@bb/mobile-bridge`, `@bb/fuzzy-match`, `@bb/tsconfig` and
`bb-plugin-automations`. They are pinned bb sources adapted in this repository
under the same ledger rules — one copy of each, never a second vendor of bb's UI.

Two boundaries are what make the app loom's rather than bb's:

* **The transport is loom's, and the seam is one small layer.**
  `apps/app/src/lib/loom-http.ts` is the lowest transport, over the route table
  in `apps/app/src/lib/loom-api-routes.ts`; `apps/app/src/lib/api-server.ts` is
  the loom-native stand-in for bb's Hono `hc<PublicApiRoutes>` client, keeping
  the `apiClient.<area>.<method>.$get(...)` call shape the ported call sites
  already use; `apps/app/src/lib/ws.ts` is the socket; and
  `apps/app/src/lib/loom-shell.ts` reads the shell's own health, sidebar
  bootstrap and system config. The React tree is what the port did *not* have to
  change.
* **The removed surfaces stay removed.** The plugin marketplace and its runtime,
  skills, the desktop-browser surface and the Electron-only shell have no route,
  no navigation entry and no runtime; a stale persisted pane is pruned rather
  than rendered. [`ui-baseline.md`](ui-baseline.md) § 产品 Surface is the matrix a
  change has to keep.

## Desktop shell: a URL plus two switches

There is no `apps/desktop` in this repository. The decision it was checked in
for still stands, and is what a shell must satisfy if one is added back:

* window/tray lifecycle and global shortcuts;
* native file dialogs and "open in editor" for a workspace on *this* machine;
* supervisoring a local `loom server` and an independently stoppable
  `loom worker`;
* auto-update and code signing.

Those are a few hundred lines around a webview that points at a URL. The rest —
the bundled UI, the local database, the worker lifecycle entangled with the
window, the in-process API server — is exactly what the fork removes, and
duplicating it in Electron would reintroduce the coupling the relay layer was
built to break.

Concretely: the shell is `{ url }` plus two supervision switches (start a local
server, start a local worker) as specified in
[`process-model.md`](process-model.md). An installed PWA already covers the
read-and-steer case for every platform, so a shell must justify each feature it
keeps — which is why nothing here needs one, and why adding one back means
adding only the shell.
