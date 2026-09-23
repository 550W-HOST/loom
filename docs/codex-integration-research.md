# Codex integration: the app server, and the ACP bridge over it

> **Status: evidence record. The decision it informs is unchanged** — loom reaches
> every agent through ACP, and Codex is no exception
> ([`provider-strategy.md`](provider-strategy.md)).
>
> The question that produced this document was whether Codex has an integration
> path that is *not* ACP — "there is a codex server, right?" There is:
> `codex app-server`. The finding worth writing down is that **Codex has no
> native ACP at all**, and the `codex-acp` bridge loom already discovers is a
> translation layer *over that same app server*. So the choice is not
> "ACP versus Codex's own protocol" as two independent integrations; it is
> "who owns the translation", and upstream already owns it.

Measured against a locally installed **`codex-cli 0.155.1`** on 2026-09-23.
Protocol facts come from schemas the CLI generates itself
(`codex app-server generate-json-schema`), from a live `initialize` /
`model/list` / `thread/list` probe against a stdio child, and from bb's
compiled `provider-codex` host bundle. Commands to reproduce are at the end.

## Status of the integration

The ACP path is implemented and verified end to end against
`@agentclientprotocol/codex-acp` **1.13.1**: `crates/worker/tests/real_codex.rs`
covers the admission catalogue (7 models, with the reasoning ladder read through
the `thought_level` category although Codex ids the option `reasoning_effort`),
a real first turn and its resume, and `session/list` plus a history replay.

No worker adapter change was needed — the generic ACP client already handles the
bridge. The work was provisioning, metadata and evidence:
[`acp-adapter.md`](acp-adapter.md) § Codex launch documents the install and
sign-in, and `crates/server/src/b10.rs` now points the `codex` tab at the bridge
rather than at the Codex CLI.

## The two paths

| | Native | Bridge (what loom uses) |
| --- | --- | --- |
| Process | `codex app-server` | `codex-acp`, which starts `codex app-server` itself |
| Wire | JSON-RPC 2.0, `"jsonrpc"` omitted | ACP over stdio |
| Who translates | loom | the `codex-acp` adapter |
| Code in loom | one provider adapter | none beyond the existing ACP client |
| Upstream | OpenAI (`openai/codex`) | `agentclientprotocol/codex-acp` |
| Where it is documented | `codex app-server` + generated schemas | the adapter's README |

`docs/provider-strategy.md` already records bb's price for the native path:
`provider-codex` was **11,539 non-test lines**, against 1,692 for bb's whole ACP
provider. This document explains what that 11,539 lines bought and what it cost,
because the number is the argument for not repeating it — and, importantly,
because most of that number is *not* event translation.

## What `codex app-server` is

The CLI's own help describes it as `[experimental] Run the app server or related
tooling`, with `generate-ts` and `generate-json-schema` subcommands. The public
API docs call it "a JSON-RPC 2.0 API that powers rich interfaces like the Codex
VS Code extension" ([overview](https://mintlify.wiki/openai/codex/api/overview)).

Facts verified locally:

- **Framing.** Bidirectional JSON-RPC 2.0 with the `"jsonrpc":"2.0"` member
  omitted on the wire. `stdio://` is one JSON object per line and is the
  default; `--listen` also accepts `unix://[PATH]`, `ws://IP:PORT`, and `off`.
  The official docs classify the WebSocket listener as experimental and
  unsupported for production.
- **Handshake.** Exactly one `initialize` per connection
  (`clientInfo{name,title,version}` + optional `capabilities`), then an
  `initialized` *notification*. Requests before it fail with `Not initialized`;
  a second `initialize` fails with `Already initialized`. The response carries
  `userAgent`, `codexHome`, `platformFamily`, `platformOs`. The `userAgent` is
  built from the client's own `name`/`version`, so a client identifies itself
  once, at handshake time. There is **no protocol-version field** to negotiate.
- **Capabilities are opt-in flags, not versions.** `capabilities.experimentalApi`
  gates experimental methods and fields; `optOutNotificationMethods` suppresses
  named notifications (`item/agentMessage/delta` is the documented example);
  `extensions` carries MCP extension settings.
- **Backpressure is part of the contract.** Saturated request ingress answers
  JSON-RPC error `-32001 Server overloaded; retry later`, which clients are told
  to treat as retryable with exponential backoff.
- **The surface is large.** `codex-cli 0.155.1` generates **164 client request
  methods** and **82 server notification methods**. Client requests include the
  thread/turn primitives below, plus account/login, config, plugins,
  marketplace, skills, apps, MCP servers, processes, filesystem, realtime
  audio, review and remote control.
- **There are two type trees.** The generator emits a stable bundle
  (`CodexAppServerProtocol`, 91 definitions) and a separate
  `CodexAppServerProtocolV2` bundle (775 definitions), with `v2/` files beside
  the legacy ones. The legacy surface still carries the old approval requests
  (`applyPatchApproval`, `execCommandApproval`) alongside the current typed ones;
  clients are expected to **generate bindings per CLI version**, not
  hand-maintain a mirror.
- **A persistent daemon exists.** `codex app-server daemon start|stop|restart|
  bootstrap|version` manages a local app-server; `codex app-server proxy --sock
  <path>` proxies stdio to its Unix socket; `codex --remote ws://…|wss://…|
  unix://…` attaches the TUI; `remote-control start|pair` exposes a pairing
  flow; `codex agents` browses sessions on the shared daemon. The daemon's
  control socket framing is *not* part of the generated schema — one third-party
  survey found `proxy` did not forward an `initialize` response
  ([protocol survey](https://raw.githubusercontent.com/KamiJeong/agent-observatory/716409cbc1fde2e5ae82e03802d871eed882c112/docs/codex-protocol.md)).

### Live probe results (this machine)

```
initialize  → {"userAgent":"loom-research/0.155.1 (Ubuntu 20.4.0; x86_64) dumb (loom-research; 0.0.1)",
               "codexHome":"…/.scratch/codex-home","platformFamily":"unix","platformOs":"linux"}
model/list  → models with supportedReasoningEfforts (low…ultra), inputModalities, serviceTiers
thread/list → {"data":[],"nextCursor":null,"backwardsCursor":null}
```

Both `model/list` and `thread/list` answered on an unauthenticated,
throwaway `CODEX_HOME` — the catalogue is readable before login, which is why
bb could fetch it on a dedicated child process.

### The primitives loom would map

| Need | App-server method |
| --- | --- |
| Create a conversation | `thread/start` |
| Continue one | `thread/resume` (loads and subscribes; `excludeTurns`) |
| Branch | `thread/fork` |
| Enumerate persisted threads | `thread/list` (cursor-paginated, filterable) |
| **Read history without resuming** | `thread/read`, `thread/turns/list`, `thread/items/list` |
| In-memory threads | `thread/loaded/list` |
| Send input | `turn/start` (`input: [text|image|localImage|…]`) |
| Cancel / add input mid-turn | `turn/interrupt`, `turn/steer` |
| Model catalogue | `model/list` (efforts, modalities, service tiers) |
| Approvals | server→client `item/commandExecution/requestApproval`, `item/fileChange/requestApproval`, `item/permissions/requestApproval`, `item/tool/requestUserInput`, `mcpServer/elicitation/request`; cleared by `serverRequest/resolved` |

`thread/read` is the structurally interesting one: it returns a thread's turns
**without resuming or subscribing to it**, so a history baseline needs neither a
second live session nor a replay guard. ACP has no equivalent — `session/load`
*is* a resume whose history arrives as a stream that must be suppressed on the
run path (see [`acp-adapter.md`](acp-adapter.md) § Loading history).

## Rust SDKs: none official for the app server

OpenAI ships exactly two SDKs, and neither is Rust
([Codex SDK](https://learn.chatgpt.com/docs/codex-sdk)):

| SDK | Package | Notes |
| --- | --- | --- |
| TypeScript | `@openai/codex-sdk` (npm) | starts/continues/resumes local threads; Node 18+ |
| Python | `openai-codex` (PyPI) | "controls the local Codex app-server over JSON-RPC"; pins its own Codex runtime |

The repository agrees: `openai/codex/sdk/` contains only `typescript`,
`python` and `python-runtime`. The SDK page's own advice for anything else is
"use the Codex app server to build custom clients" — i.e. write the client —
and it confirms the older integration surface is gone: *"The `codex mcp-server`
command and standalone `codex-mcp-server` binary have been removed. Use the Codex
app server for existing integrations."*

The app-server's types *are* Rust — they live in OpenAI's workspace as
`codex-rs/app-server-protocol` (lib `codex_app_server_protocol`), alongside
`codex-protocol`, `codex-history`, `codex-rollout` and friends — but they are
unpublished internal crates, not an SDK. The crates.io names are held by someone
else: `codex-protocol` and `codex-app-server-protocol` at **0.63.0**, owned by
`namastex888`, repository `namastexlabs/codex`, published 2025-12-11. That is a
third-party fork's snapshot — 0.63.0 against today's `codex-cli 0.155.1` — and it
should not be read as an official package.

What the community offers instead (all unofficial, all needs vetting):

| Crate | Where | Shape |
| --- | --- | --- |
| `codex-codes` 0.156.1 | `meawoppl/rust-code-agent-sdks` | Typed serde models of the app-server JSON-RPC protocol plus sync and async (Tokio) clients; the version names the CLI release its live integration suite passed against; `types` feature is WASM-compatible |
| `codex-app-server-sdk` 0.5.1 | `thehumanworks/codex-sdk-rs` | "Tokio Rust SDK for Codex App Server" |
| `codex-wrapper` 0.4.4 | `joshrotenberg/codex-wrapper` | Type-safe wrapper over the `codex` CLI (not the protocol) |
| `arm64be/codex-rs` | GitHub only | Auto-generated Rust bindings for the app server; 0 stars, last push 2026-08-18 |

For loom's actual path there *is* an official Rust SDK, and it is already a
dependency: **`agent-client-protocol`** (repo
[`agentclientprotocol/rust-sdk`](https://github.com/agentclientprotocol/rust-sdk)),
with `agent-client-protocol-schema`, plus `-http`, `-rmcp`, `-derive`,
`-conductor`, `-polyfill`, `-test` and `-cookbook` crates. It is maintained under
the protocol organisation (crates.io owners include the
`agentclientprotocol:rust-maintainers` team) and is the stable-v1 / draft-v2
implementation loom's worker drives. Note the asymmetry: the ACP **client** SDK
is official Rust, while `codex-acp` itself is a TypeScript program.

## The ACP bridge is a wrapper over the same app server

The current adapter's README is explicit
([`agentclientprotocol/codex-acp`](https://github.com/agentclientprotocol/codex-acp)):

> `codex-acp` is a stdio ACP agent server. It starts the Codex App Server,
> translates ACP requests into Codex operations, and maps Codex events back into
> the client.

Two operational facts follow:

- **The package moved.** Zed's original adapter is deprecated in favour of
  `@agentclientprotocol/codex-acp`; the old `@zed-industries/codex-acp` README
  now points there. Both install a binary named **`codex-acp`**, which is the
  exact command `crates/worker/src/discovery.rs:96` probes — so loom's discovery
  keeps working across the move, but its branding data still describes the
  Codex CLI rather than the bridge (see "Recommendation").
- **The bridge ships its own Codex.** The npm package declares a compatible
  `@openai/codex` dependency; `CODEX_PATH` overrides it. A host's `codex` on
  `PATH` and the bridge's bundled Codex can therefore be different versions, and
  the version skew is the bridge's problem, not loom's.

The new adapter advertises a fuller ACP surface than one might expect. Read from
`src/CodexAcpServer.ts` at `main`:

```
agentCapabilities: { loadSession: true,
  promptCapabilities: { embeddedContext: true, image: true },
  sessionCapabilities: { resume, list, close, delete, fork,
                         additionalDirectories, subagents },
  auth: { logout }, providers: {}, mcpCapabilities: { http: true } }
```

with `loadSession`, `forkSession` and `listSessions` implemented as ACP
methods. So the capabilities loom's design leans on — resume, history replay,
`session/list` for import, fork, images, embedded context — are all present on
the ACP path already, and the app-server work behind them belongs to upstream.

One consequence for discovery: the adapter authorizes before it serves a
session, so an unauthenticated host fails loom's enrollment probe and simply
does not advertise `codex`. That is the existing rule — admission is a real ACP
session that answers — not a new failure mode.

## Why loom's event vocabulary already looks like Codex's

bb's `ThreadEvent` contract — which loom adopted as its 35-type
`ProviderEvent` — was modelled on the app-server notification vocabulary.
The overlap is nearly exact:

| `ProviderEvent` (`crates/domain/src/provider_event.rs`) | app-server notification |
| --- | --- |
| `thread/started`, `turn/started`, `turn/completed` | same |
| `item/started`, `item/completed` | same |
| `item/agentMessage/delta` | same |
| `item/commandExecution/outputDelta` | same |
| `item/fileChange/outputDelta` | same |
| `item/reasoning/summaryTextDelta`, `item/reasoning/textDelta` | same |
| `item/plan/delta`, `item/mcpToolCall/progress` | same |
| `thread/tokenUsage/updated`, `thread/name/updated`, `thread/compacted` | same |
| `turn/plan/updated`, `turn/diff/updated` | same |
| `provider/error`, `provider/rateLimits/updated`, `provider/warning` | `error`, `account/rateLimits/updated`, `warning` |

This is the strongest technical argument *for* the native path: at the event
layer its mapping is nearly free, whereas ACP's coarser model forces the adapter
to synthesize turns, mint item ids, and choose item variants from partial tool
data (the hardest parts of [`acp-adapter.md`](acp-adapter.md)).

The counter-argument is what the 11,539 lines actually were. From bb's compiled
`provider-codex` host bundle:

- **One `codex app-server` child per thread**, plus a second long-lived child
  just for `model/list`, plus short-lived "maintenance" children for
  archive/rename/goal-clear. Every settings change, child exit, or auth recovery
  kills and respawns the child and replays the rollout (`thread/resume`,
  `excludeTurns:true`).
- Rebuild is triggered by a `constructionSignature` hash over cwd, model,
  reasoning effort, memory, approvals, sandbox and pool route — a session is
  disposable and reconstructible, and bb has to prove when.
- Approvals are decoded only for three of codex's request methods; everything
  else answers `METHOD_NOT_FOUND`.
- Error handling is regex classification of agent prose (`is archived`,
  `no rollout found`, `rollout at … is empty`, `401/403|auth`, `429|credits|
  quota|rate-limit`) with recovery hints.
- Version pins: minimum Codex `0.136.0`, rewind minimum `0.143.0`, plus a
  bridge protocol version and delta-grammar version of its own.

None of that is translation. It is *lifecycle and failure plumbing* that
`codex-acp` already contains — which is exactly the "narrow grammar" division of
labour `provider-strategy.md` describes, except paid upstream.

## What native would add over the bridge today

Honest list, assuming the bridge continues to cover the ACP surface above:

- `thread/read` + `thread/turns/list` + `thread/items/list`: paginated history
  without resuming, subscribing, or replaying.
- `thread/fork`/`rollback`/`revert`, `turn/steer` with `expectedTurnId`, and the
  queue/section/search APIs around threads.
- Subagent graph metadata as protocol facts (`collabAgentToolCall`,
  `subAgentActivity`, `parentThreadId`), rather than reconstructed from ACP
  subagent sessions.
- `model/list` reasoning-effort ladders and service tiers, `skills/*`,
  `plugin/*`, `app/*`, realtime voice, process and filesystem RPCs.
- The daemon shape: one app-server, many clients, over a Unix socket — arguably
  a better fit for a worker that runs many threads than bb's process-per-thread.

## What it would cost loom

- **A second provider protocol.** `loom-provider-protocol` and the worker would
  carry an app-server adapter beside the ACP client, and Codex would become the
  one agent the ACP rule does not cover — the special case
  `provider-strategy.md` explicitly refuses.
- **No official Rust SDK to build it on.** OpenAI's SDKs are TypeScript and
  Python; the Rust crates that model the protocol are unpublished internals, and
  the crates.io names belong to a third-party fork. A Rust worker would either
  hand-generate bindings from `generate-json-schema` or adopt an unofficial
  community crate — a dependency decision loom would be making for one agent.
- **Generated bindings as a dependency of the build.** 164 requests, 82
  notifications, two type trees, an experimental gate, and a schema that is
  regenerated per CLI release. A hand-written mirror rots; a generated one makes
  loom's protocol surface track Codex's release cadence.
- **Auth and discovery become loom's problem.** The bridge advertises ACP auth
  methods (ChatGPT login, `CODEX_API_KEY`, custom gateway) and handles them.
  Native means implementing `account/login/*` and the provider/auth-recovery
  state machine bb had to build.
- **A process model decision.** Either one app-server child per run thread (bb's
  shape, with the rebuild-on-exit cost above) or the shared daemon, whose
  control-socket framing is outside the generated schema and whose WebSocket
  listener the vendor calls experimental.
- **Two codex paths is a fallback by another name.** Under the "no fallbacks"
  rule, a native path that exists alongside `codex-acp` has to justify itself as
  the *only* path, not as a spare.

## Recommendation

Keep the single ACP path. Concretely:

1. `codex` in `KNOWN_AGENTS` stays `codex-acp`; no native Codex adapter is
   added. The 11,539-line precedent is the price of owning a translation that
   upstream now maintains for free.
2. **Fix the branding/discovery metadata to describe the bridge, not the CLI.**
   `crates/server/src/b10.rs:284` currently gives provider `codex` the sign-in
   command `codex` and the install page
   `https://developers.openai.com/codex/cli`. What must be installed for this
   provider to appear is the bridge
   (`npm install -g @agentclientprotocol/codex-acp`), and what signs it in is
   Codex's own auth (`codex login`) or `OPENAI_API_KEY`/`CODEX_API_KEY` through
   the adapter. The current values are not *wrong* — the bridge needs Codex —
   but a host that follows them literally installs a CLI loom never launches.
3. Record the bridge's provenance in the discovery comment (already correct)
   and note the packaged-Codex/`CODEX_PATH` skew so a future version-pin
   question has a starting point.
4. Keep probing rather than assuming: `codex-acp` is a moving upstream, and
   loom's enrollment probe already re-reads its capabilities on every
   enrollment. A capability that appears there needs no loom change.

### Triggers that would reopen the native decision

- `codex-acp` becomes unmaintained, or lags Codex releases long enough that
  newly shipped Codex features cannot reach loom.
- A capability loom needs that ACP cannot express *at all* — the clearest
  candidate is `thread/read`-style history that must not resume or subscribe.
  (Today the bridge's `session/load` covers display; the concern is only whether
  a resumed load is ever unacceptable.)
- The bridge becomes a compatibility tax: many pinned bridge versions per Codex
  version, or a host where the bundled Codex cannot be used and `CODEX_PATH`
  skew causes failures.

If it is ever reopened, the mapping is the cheap part. Start from
`thread/start|resume|list|read`, `turn/start|interrupt`, the approval requests
bb decoded (`item/commandExecution/requestApproval`,
`item/fileChange/requestApproval`, `item/permissions/requestApproval`), and the
notifications loom's `ProviderEvent` already names — not from the full
164-method surface.

## How to reproduce

```sh
codex --version                                   # codex-cli 0.155.1
codex app-server --help
codex app-server generate-json-schema --experimental --out .scratch/codex-schema
codex app-server generate-ts --out .scratch/codex-ts
python3 .scratch/codex_probe.py                   # initialize / model/list / thread/list
```

`.scratch/` is gitignored; the probe writes generated output and a throwaway
`CODEX_HOME` there. The bb evidence is the compiled plugin at
`~/.bb/plugin-host-artifacts/provider-codex/<hash>/host.mjs` (source-map paths
name the original `plugins/provider-codex/src/*.ts` files).

## References

- `codex app-server` generated schema, `codex-cli 0.155.1` — `ClientRequest`
  (164 methods), `ServerNotification` (82 methods),
  `codex_app_server_protocol.v2.schemas.json` (775 definitions)
- [Codex App Server API overview](https://mintlify.wiki/openai/codex/api/overview),
  [initialization](https://mintlify.wiki/openai/codex/api/initialization),
  [threads](https://mintlify.wiki/openai/codex/api/threads),
  [turns](https://mintlify.wiki/openai/codex/api/turns),
  [items](https://mintlify.wiki/openai/codex/api/items),
  [models](https://mintlify.wiki/openai/codex/api/models)
- [Agent Observatory protocol survey](https://raw.githubusercontent.com/KamiJeong/agent-observatory/716409cbc1fde2e5ae82e03802d871eed882c112/docs/codex-protocol.md)
  — transports, discovery, daemon caveats, status projection
- [`agentclientprotocol/codex-acp`](https://github.com/agentclientprotocol/codex-acp)
  — README and `src/CodexAcpServer.ts` (capabilities, `loadSession`,
  `listSessions`, `forkSession`)
- [Zed: Codex is Live in Zed](https://zed.dev/blog/codex-is-live-in-zed) —
  the original `@zed-industries/codex-acp` adapter
- [Codex SDK](https://learn.chatgpt.com/docs/codex-sdk) — the official
  TypeScript and Python SDKs, and the `codex mcp-server` removal notice
- [`agentclientprotocol/rust-sdk`](https://github.com/agentclientprotocol/rust-sdk)
  — the official ACP Rust SDK (`agent-client-protocol` and friends)
- crates.io — `codex-protocol` / `codex-app-server-protocol` 0.63.0
  (owner `namastex888`, repo `namastexlabs/codex`); `codex-codes`,
  `codex-app-server-sdk`, `codex-wrapper`
- `docs/provider-strategy.md` — the ACP-only decision and bb's per-provider line counts
- `docs/acp-adapter.md` — what the ACP adapter owns, and the history/load boundary
- `crates/worker/src/discovery.rs` — the known-agent table that probes `codex-acp`
- `crates/server/src/b10.rs` — provider branding (marks, sign-in, install page)
