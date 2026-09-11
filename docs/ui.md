# UI as a URL client

The UI holds no state and owns no server address. It is a set of static assets
served by `loom-server` from the same origin as the API, so the only thing a
client needs is the URL:

```
UI (browser / PWA / desktop webview) ── HTTP + WS ──▶ loom-server ── relay ──▶ frames
                                                          ▲
                                                       daemon (separate process)
```

A UI never talks to a daemon, and a daemon never talks to a UI. Pointing a
client at a URL is the whole configuration.

## Hosting

`loom-server` answers the fallback route from one of three sources, chosen by
environment. They are mutually exclusive, because two UI sources would make the
result depend on evaluation order.

| Source | Selected by | Use |
| --- | --- | --- |
| Embedded reference client | default | Zero-configuration `cargo run -p loom-server`; a working UI at `/` |
| Built bundle on disk | `LOOM_UI_DIR=/path/to/dist` | Production; this is where the ported bb bundle is served from later |
| Dev-server proxy | `LOOM_UI_PROXY=http://127.0.0.1:5173` | Frontend development with hot reload against the real server |

Rules that apply to every source:

* A request for a **client route** (no file extension) that has no matching file
  falls back to `index.html`, so history routing works.
* A request for an **asset** (has an extension) that does not exist returns
  `404`. A missing script must not silently become an HTML page.
* A **non-GET/HEAD** fallback request returns `404` (the proxy forwards
  methods as-is, for the dev server).
* `/api/**` and `/ws` never fall through to the UI. A mistyped API route is a
  JSON `404`, not the SPA shell with a `200`. This is the failure mode that
  makes an API error look like a UI bug, so it is refused explicitly.
* Directory paths are never normalised silently: `..`, absolute paths and
  prefixes are rejected instead of clamped, so a bug cannot become a sandbox
  escape.

The proxy tunnels WebSocket upgrades as well as HTTP, so Vite's HMR socket keeps
working while `/api` and `/ws` stay on `loom-server`. During development the UI
therefore still uses one origin and needs no CORS configuration.

## The client contract

The reference client (`ui/`, compiled into the server) is the living
specification for any client, including the ported bb UI. It uses only the
public server surface:

1. **Derive the server from the origin.** `const base = window.location.origin`
   and `ws(s)://location.host/ws`. There is no server-address setting anywhere.
2. **List threads over HTTP.** `GET /api/v1/threads` returns
   `{ "threads": [Thread, ...] }`, newest first.
3. **Open a thread on the socket.** Connect to `/ws`, then send
   `{"type":"subscribe","scope":{"kind":"thread","id":"<id>"}}`. The same scope
   a producer published to; no handler is involved.
4. **Subscribe first, then replay.** After the subscription is acknowledged,
   call `GET /api/v1/replay?scope_kind=thread&scope_id=<id>&since=<last_event_id>`.
   The order is load-bearing: a frame published between the two calls arrives
   live *and* is in the window, and the client drops the duplicate by
   `event_id`. Fetching the backlog first would instead miss whatever was
   published in the gap, and a missed frame is unrecoverable.
5. **Persist the cursor.** The last rendered `event_id` is the resume cursor.
   The reference client stores it per scope in `localStorage` under
   `loom:last-event:<kind>:<id>`.
6. **Merge and order by event id.** Replay and live frames are the same bytes
   (`docs/architecture.md` § "What is stored"), so no shape translation is
   needed. Event ids are fixed-width monotonic ULIDs, so sorting by id restores
   chronological order when the two streams interleave.

A payload is a serialized `loom-domain` `DomainEvent`: dispatch on its `type`
tag. The events a thread view renders are `thread_message_added`,
`thread_status_changed` and `thread_run_event` (`crates/domain/src/event.rs`).

The round trip the reference client proves end to end:

```
GET /api/v1/threads            → thread list
  → open thread
  → WS subscribe thread:{id}
  → GET /api/v1/replay?since=…  → backlog, merged by event_id
  → POST /api/v1/threads/{id}/messages (ack only)
  → live thread_message_added frame on the socket → render
```

## Porting the bb UI

The bb app is checked in next, but deliberately not in this step. The reason is
that "the existing bb UI builds" and "the existing bb UI talks to loom" are two
different problems, and only the first is a mechanical copy.

What `apps/app` needs to build, from the bb tree:

* 1526 files, built with Vite under a pnpm + turbo workspace.
* About twenty workspace packages: `@bb/sdk`, `@bb/client-core` (the API
  client), `@bb/core-ui`, `@bb/shared-ui`, `@bb/thread-view`, `@bb/domain`,
  `@bb/config`, `@bb/server-contract`, `@bb/host-daemon-contract`,
  `@bb/desktop-contract`, `@bb/templates`, and their transitive peers.

What it needs to *run* against loom is smaller than it looks, because bb and
loom already agree on the shape (origin-derived base URL, a socket, a frame
stream). It does **not** agree on the API: the bb UI calls the bb server
contract, which loom has not implemented. The seam is one module,
`apps/app/src/lib/api-server.ts` / `@bb/sdk`, not the React tree.

The phased plan, smallest valuable step first:

1. **Done, this issue.** Server hosts the UI, the UI subscribes to
   `thread:{id}` through the relay, and reconnect merges live and replayed
   frames. The reference client proves the path.
2. **Check in `apps/app` and its workspace closure verbatim**, plus
   `pnpm-workspace.yaml`, the root `package.json`, `turbo.json` and the
   `packages/*` it needs. Change nothing but the workspace name. Prove
   `pnpm --filter @bb/app build` passes and serve `apps/app/dist` with
   `LOOM_UI_DIR`. This is the mechanical step; it is large but low-risk.
3. **Adapt the transport, not the tree.** Replace `@bb/sdk`'s fetch/WS base with
   loom's routes (`/api/v1/...`, `/ws`) and map the thread list and thread
   timeline onto bb's query layer. Ship one screen — list → thread → messages —
   before anything else.
4. **Fill in routes by screen**, in order of use. The 412-route surface is
   reached by porting one screen at a time, never by porting the server
   contract wholesale.

Do not fold step 2 into this issue: importing a monorepo is a review of its own,
and a broken import would obscure the verified path above.

## Desktop shell: keep, but reduce to a shell

`apps/desktop` (Electron, ~29k lines) should be **kept in the tree but reduced
to a thin shell**, not deleted and not maintained at its current size.

Once the UI is a URL client, the desktop shell's only incremental value is what
the browser cannot do locally:

* window/tray lifecycle and global shortcuts;
* native file dialogs and "open in editor" for a workspace on *this* machine;
* supervisoring a local `loom-server` and an independently stoppable
  `loom-daemon`;
* auto-update and code signing.

Those are a few hundred lines around a webview that points at a URL. The rest —
the bundled UI, the local database, the daemon lifecycle entangled with the
window, the in-process API server — is exactly what the fork removes, and
duplicating it in Electron would reintroduce the coupling the relay layer was
built to break.

Concretely: the shell becomes `{ url }` plus two supervision switches (start a
local server, start a local daemon) as specified in
[`process-model.md`](process-model.md). An installed PWA already covers the
read-and-steer case for every platform, so the shell must justify each feature
it keeps. This issue does not delete anything; the trim is its own change with
its own review.
