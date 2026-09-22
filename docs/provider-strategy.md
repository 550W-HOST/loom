# Provider strategy: one ACP path

Decision record. loom standardises every agent on ACP. Pi is not special-cased.

## What was decided

1. **All agents are reached through ACP.** One adapter, N agents.
2. **`loom resume <thread>` is the only resume entry point.**
3. **No provider-specific fallback paths.** ACP protocol negotiation may select
   v1 when an agent does not speak v2; an unsupported capability is reported,
   not worked around.

## Why not copy bb

bb supports four providers with four different integrations:

| provider | non-test lines | integration | SDK |
| --- | --- | --- | --- |
| `provider-pi` | 6,099 | JSON-RPC over stdio (`pi --mode rpc`) | `@earendil-works/pi-coding-agent` |
| `provider-acp` | 1,692 | ACP over stdio | MCP SDK |
| `provider-claude-code` | 10,973 | Claude SDK (`query()`) | — |
| `provider-codex` | 11,539 | `codex app-server` JSON-RPC | — |

ACP is one of four, and it is the cheapest of the four by an order of
magnitude (1.7k lines for 5 agents, against 11k for one). bb pays for the other
three because it needs provider-native features each SDK exposes, and because
it ships a plugin runtime where third parties write bridges.

loom has neither constraint. Its advantage is the opposite one: **`pi-acp`
already exists**, so routing Pi through ACP costs nothing extra and buys the
same path every other agent uses.

### What bb does have that is worth copying

Not the four integrations — the *convergence point*. bb's
`provider-bridge-protocol` defines a narrow grammar (`thread/delta`, ~20 kinds:
`input.accepted`, `turn.open`, `item.open`, `item.delta`, `item.close`, …) and
the documented division of labour:

> **the bridge knows the dialect, the runtime knows the timeline.** A bridge
> parses its provider's traffic into a narrow grammar of semantic deltas
> (`thread/delta`); the runtime's delta assembler owns every timeline invariant
> — id minting, turn/item lifecycle, ordering — and constructs the canonical
> `ThreadEvent`s.

The reason to note this: **ACP already is that narrow grammar.** bb had to
invent `thread/delta` because four integrations had four shapes. With one
protocol, the protocol's own `SessionUpdate` plays the role. loom writes no
translator between provider dialects — only one mapping from ACP to its event
model.

## What ACP provides

### Session lifecycle

| Method | v1 | v2 | Purpose |
| --- | --- | --- | --- |
| `session/new` | yes | yes | Create; the agent returns a `sessionId` |
| `session/load` | yes | **no** | Restore by id, replaying history |
| `session/resume` | yes | yes | Restore by id; v2 adds a replay cursor |
| `session/list` | yes | yes | Enumerate, with a `cwd` filter and cursor pagination |
| `session/delete` | yes | yes | Remove |
| `session/fork`, `session/close` | yes | yes | Branch, release |
| `session/set_mode` | yes | **no** | v2 replaced modes with config options |
| `session/set_config_option` | yes | yes | Session configuration |

The `session/load` row is the sharpest difference and the reason the negotiation
matters: the method `loom resume` was designed around does not exist in v2. v2's
`session/resume` with `replayFrom: {"type":"start"}` is its equivalent, and the
conversion layer maps them onto each other.

`ReplayFrom` has only two variants — `Start` and an untagged `Other` for
forward compatibility — so "replay from an arbitrary point" is not expressible.
An adapter that receives an unknown cursor must reject it rather than guess,
as the schema itself instructs.

Capabilities are advertised in `initialize`. The shape differs by version:
v1 uses a flat `agentCapabilities.loadSession: bool` plus
`sessionCapabilities.{list,delete,resume}`, while v2 has no `loadSession` at all
(it is implied by `session/resume`) and nests capabilities under `session`.

### Streaming and timeline

The union of what the two versions can carry:

```
UserMessageChunk / UserMessage          ← the non-chunked forms are v2 only
AgentMessageChunk / AgentMessage
AgentThoughtChunk / AgentThought
ToolCall / ToolCallUpdate               ← v1's only patchable pair
ToolCallContentChunk
TerminalUpdate / TerminalOutputChunk    ← v2 only
Plan / PlanUpdate / PlanRemoved
StateUpdate                             ← v2 only
UsageUpdate
SessionInfoUpdate
AvailableCommandsUpdate / ConfigOptionUpdate
CurrentModeUpdate                       ← v1 only; skipped when talking v2
```

The `CurrentModeUpdate` asymmetry is the one lossy edge in the conversion layer:
v2 dropped modes in favour of config options, so the adapter omits it on the v2
path rather than failing the turn.

Two properties matter for loom's timeline:

- **Chunked variants stream.** `AgentMessageChunk`, `AgentThoughtChunk`,
  `ToolCallContentChunk`, `TerminalOutputChunk` are deltas, so text arrives
  incrementally rather than as whole messages. Both versions have the message
  and thought chunks; v1 has no terminal or tool-content chunks.
- **The non-chunked variants are full objects with ids, and repeat updates patch
  by id.** The schema documents this: *"When a client receives another
  `agent_message` update with the same `messageId`, fields in the new update
  patch the previous fields for that message."* In v1 only `ToolCall` /
  `ToolCallUpdate` work this way; the message objects are v2's addition.

The second is what makes an event log work: an item's identity is stable across
updates, so a log can carry patches and a consumer can converge. Under ACP the
exact form of that patch comes from the schema:

> `content` has patch semantics: an omitted field leaves existing message
> content unchanged, `null` clears the value, and a concrete array replaces the
> previous value.

A useful consequence: a **resumed** log needs only the newest full object per
item to converge, not every chunk that built it.

### v1, v2, and why loom negotiates rather than picks

There are two complete, mutually incompatible type trees in
`agent-client-protocol-schema`: `v1` and `v2`. They are not one tree with a
version field, and types from one do not interoperate with the other.

What v2 adds over v1:

- `StateUpdate` (`Running` / `Idle` / `RequiresAction`)
- `AgentMessage` / `AgentThought` / `UserMessage` — **patchable full objects**
  (v1 has only `ToolCall` / `ToolCallUpdate`)
- `TerminalUpdate` / `TerminalOutputChunk` (agent-owned terminals)
- `Other` (`OtherSessionUpdate`) as a typed catch-all for forward compatibility
- structured multi-file diffs (`DiffPatch` et al.; v1 has only `Diff`)
- `session/resume` replaces `session/load`, and prompt completion moves from
  `PromptResponse.stop_reason` to `StateUpdate::Idle`

Three facts constrain the choice, all verified against the crates and against
`pi-acp` rather than assumed:

**1. v2 is an unstable draft.**

```rust
// schema 1.5.0, src/lib.rs:44
#[cfg(feature = "unstable_protocol_v2")]
pub mod v2;

// src/version.rs:49 — without the feature, LATEST is v1
#[cfg(not(feature = "unstable_protocol_v2"))]
pub const LATEST: Self = Self::V1;
```

**2. v1 has no `Other` catch-all.** `v1::SessionUpdate` is a plain tagged enum;
an unknown `sessionUpdate` value fails to deserialise. So under v1 the "store
unknown updates" rule must be implemented at the **raw JSON-RPC layer**
(`UntypedMessage`), before typed dispatch — not at the `SessionUpdate` level.

**3. The ecosystem is on v1, and `pi-acp` now speaks both.** A client that
refuses what the agent answers cannot talk to it at all. `pi-acp` `v0.5.0`
serves v2 natively — v1 and v2 are separate implementations selected in
`initialize` — while the other agents in scope still speak only v1, which is why
loom negotiates rather than picking one.

So loom **negotiates**, using the SDK's own connector:

```rust
// agent-client-protocol 2.0.0, src/role/acp.rs:266
Client::protocol_connector()
    .with_v1(|| my_v1_client())
    .with_v2(|| my_v2_client())
    .connect_to(agent)
```

It tries v2 first, and when the agent answers v1 it replays `initialize` on the
same connection and switches (acp.rs:183-206) — a negotiation, not a reconnect.

**This is not the fallback the no-fallback rule forbids.** Refusing to degrade
means *report what cannot be done rather than fake it*. Choosing the stable
version that the agent actually speaks is not degradation; and v1 is the version
that has `session/load`, which is what `loom resume` needs.

The SDK handles version selection and initialization-level conversion, but note
that **after `initialize` it pipes frames through without converting them**
(`pipe_protocol_peers_until_done`, acp.rs:828). Per-message conversion is the
adapter's responsibility.

`agent-client-protocol-schema`'s `v2::conversion` module supplies that
conversion **in both directions** — 207 v2→v1 and 200 v1→v2 implementations,
gen against `try_v2_to_v1` / `try_v1_to_v2`. One variant is lossy:
v1's `CurrentModeUpdate` has no v2 equivalent (v2 replaces modes with config
options) and conversion errors. The adapter skips it on the v2 path.

The patchable full objects are the reason v2 is worth the negotiation: repeated
updates for the same `messageId` are applied as patches, so an event log can
carry corrections and a consumer converges. `pi-acp` emits them: W-562 shipped
v2 behind an off-by-default `protocol-v2` feature, and `v0.5.0` **removed that
feature** — both protocol implementations are now always compiled and one is
selected per connection in `initialize`.

## The resulting loom architecture

```
loom worker (one process)
  │
  ├─ ACP client                        ← loom's only provider code
  │    ├─ Channel::duplex() ──▶ pi-acp as a lib ──▶ pi (child process)
  │    └─ Stdio::new()      ──▶ omp / hermes / cursor (child processes)
  │
  ├─ (thread) -> (agent, session_id, cwd)   in the domain snapshot
  │
  ▼
loom's event model
      │
      ▼
relay (per-thread scope)          ← already built
      │
      ▼
UI projection (thread-view)       ← already ported
```

### Pi is embedded, not spawned

Pi does not speak ACP natively, so something must translate its `--mode rpc`
JSON-RPC into ACP. That translator is `pi-acp`, and it is **linked into the
worker as a library** rather than run as another process.

The framework already supports this. `ConnectTo`'s provided method:

```rust
// agent-client-protocol 2.0.0, src/component.rs:142
fn into_channel_and_future(self) -> (Channel, BoxFuture<'static, Result<()>>) {
    let (channel_a, channel_b) = Channel::duplex();
    let future = Box::pin(self.connect_to(channel_b));
    (channel_a, future)
}
```

`Channel` is a connected pair of unbounded mpsc endpoints
(`src/jsonrpc.rs:5636`). In-process connection is therefore an intended
capability, not a workaround.

Why this shape rather than rewriting Pi support in loom:

| | Embedded `AcpAgent` (chosen) | Rewrite Pi support in loom |
| --- | --- | --- |
| Pi translation logic | one copy, shared with Zed | a second copy to maintain |
| Pi needs an extra process | no | no |
| ACP client code in loom | one path for every agent | plus a Pi-specific path |
| pi-acp changes required | one transport-injection entry point | none, but loom duplicates it |

The cost is one small change in `pi-acp`: an entry point that accepts
`impl ConnectTo<Agent>` so a caller can supply a channel instead of stdio.
`run()` keeps its signature and behaviour, so Zed is unaffected.

(The bound is `ConnectTo<Agent>`, not `ConnectTo<Client>`: in
`agent-client-protocol` 2.0.0 the parameter is the *counterpart* role, matching
the SDK's own `AgentProtocolRouter::connect_to(client: impl ConnectTo<Agent>)`.
This was settled while implementing the change — see `pi-acp` W-559, which
shipped `AcpAgent::run_with` with that bound.)

**The property that survives either way**: loom's ACP client does not know which
kind of peer it is talking to. An embedded library and a spawned agent differ
only in which `ConnectTo` implementation is handed to it.

Responsibilities:

- **The adapter** owns the ACP client side: spawn, `initialize`, session
  lifecycle, and mapping `SessionUpdate` to loom events. It does not own
  timeline invariants beyond what ACP gives it.
- **The domain** owns `(thread) -> (agent, session_id, cwd)`. Written on
  `session/new`, read on resume.
- **The relay** carries the events. Nothing about ACP is visible above the
  adapter.

### Resume

There are two ways a session is restored, and they want opposite things from
the agent:

```
loom resumes a session to CONTINUE it (a run is dispatched)
  → look up (agent, session_id, cwd) in domain state
  → start the agent and negotiate a protocol version
  → v2: session/resume { sessionId, cwd }        (no replay — a run needs no past)
     v1: session/load   { sessionId, cwd }        (the only option; replays)
  → the agent restores its own storage, and run-time history is suppressed

loom loads a session to SHOW it (a thread is opened)
  → same lookup, on a connection of its own
  → v2: session/resume { sessionId, cwd, replayFrom: start }
     v1: session/load   { sessionId, cwd }        (always replays)
  → the replay is collected as the conversation; no prompt is ever sent
```

To continue a session, loom asks for **no replay**: it holds the turn it is
about to take, and under v2 a restored session plus patchable full objects
converges without the agent repeating its past. Under v1 there is no such
choice — `session/load` is the only restore method, and it replays, so the
run-time path suppresses the history frames.

To *display* a conversation, replay is the point: the server holds no durable
copy of it (see [`architecture.md`](architecture.md) § The conversation is not
in the log), so opening an old thread loads the agent's own history over a
dedicated connection and caches it for display. The two paths never share a
connection: a load that is only reading must not race a turn that is writing.

Either way, **loom never reads an agent's session files.** It asks the agent.
That is the property that makes the design clean: no format parsing, no layout
assumptions, no per-agent storage code.

A resumed session's `cwd` must still exist. The check is the adapter's, since
only it knows the agent's rules, and the failure is explicit rather than a
silently fresh session (see "No fallbacks"). A session is only resumed by the
**host that owns it**: the stored binding carries a host id, and a dispatch or a
load for a different host fails rather than starting a fresh conversation in a
session that cannot be there.

### Importing existing sessions

The Zed-like flow, and the only sanctioned form of "reading":

```
loom threads import --agent pi --cwd <path>
  → session/list { cwd }
  → the agent enumerates its own storage
  → user picks; loom creates a thread bound to that session_id
```

`pi-acp` already implements this: it walks `~/.pi/agent/sessions/`, reads each
file's header for `cwd`, and filters. **The reading lives in the agent adapter,
not in loom** — which is where it belongs, because the adapter is the thing that
knows the format.

## No fallbacks

Consequences that must be enforced rather than papered over:

| Situation | Behaviour |
| --- | --- |
| Agent does not advertise the selected version's resume capability | `loom resume` reports it is unsupported. No copy-a-file or private-storage fallback. |
| Agent does not advertise `session/list` | The agent does not appear in the import list. loom does not scan a guessed directory. |
| A resumed session's `cwd` no longer exists | Explicit error. bb's wording is a good model: *"Cannot resume: the session's working directory `<path>` no longer exists."* Never silently start a fresh session. |
| Agent process dies mid-turn | The turn ends in a terminal state with the reason. No automatic replay into a new session. |
| Unmapped update type | **Stored and logged, never coerced.** The mechanism differs by version — see below. |

The last row is the same rule W-538 applied to `provider/unhandled`: an
unmapped frame is reported, never given a catch-all body.

## Migration

Current state: loom depends on `pi-acp` and the ACP SDK. `ProviderLaunch` has
only two ACP forms: `AcpEmbeddedPi` for Pi and `AcpStdio` for native agents.
`crates/worker/src/provider.rs` contains only run metadata and terminal-event
construction; the old `effective_argv` and direct Pi JSON-RPC mapper are gone.
The server persists the opaque provider session id — and the host that owns it
— in the thread snapshot, and carries it on the next `RunDispatch`. The ACP
driver uses `session/resume` for a resumed v2 session without replay and
`session/load` for v1, suppressing history notifications from the new run in
both cases. The separate history-load path is the opposite: it asks for the
replay and refuses everything else (see [`acp-adapter.md`](acp-adapter.md)
§ Loading history).

The current implementation negotiates ACP v2 first and falls back to v1 through
the SDK connector. The v2 schema is still unstable, so v1 remains a required
compatibility path and the v2-specific shapes are contained in the worker
adapter. The default Pi path can therefore continue to negotiate v1 while native
agents that support v2 use message patches, terminal updates and idle
completion.

## Discovery, not configuration

Which agents a machine offers is a property of that machine, so it is found
rather than declared. There is no environment variable and no config key that
lists providers: the worker carries a table of agents loom knows how to launch
(`crates/worker/src/discovery.rs`), resolves each one on its own `PATH`, and
reports what it found. Installing an agent *is* the provisioning step, and
uninstalling one removes it on the next enrollment.

Presence is necessary but not sufficient. Each candidate is then probed with a
real ACP session — `initialize`, `session/new`, and the config options it
publishes when available — and only an agent that answers is reported. A binary
that shares a name with a known agent, an agent that is installed but broken,
and a bridge package that was never installed are all simply absent from the list
instead of offered and failing at the first user turn.

The probe negotiates both protocol versions. Both versions may publish config
options, so the worker reads a v1 model selector when an agent supplies one;
an agent without a model selector is still admitted with an empty catalogue.
That is also why admission cannot be *the catalogue read*: a successful ACP
session is the liveness proof, while catalogue data is an optional dividend.
The native agents in the field — OMP, Hermes, OpenCode — speak v1, and their
v1 capabilities must not be mistaken for a failed handshake. Pi does negotiate
v2, which is what still gives it a real model ladder at enrollment.

Answers are reported as they arrive rather than in one batch at the end, so a
single agent that never completes its handshake — one waiting on a login, or a
bridge that hangs — delays only its own appearance, not every other agent's.

```
worker PATH lookup ──▶ candidate specs ──▶ ACP handshake ──▶ HostProviders
   (known-agent table)                    (the probe)        (verified list)
                                                                   │
                              server records it per host ◀─────────┘
                                                                   │
                    /system/providers + executionOptions ◀─────────┘
```

Two properties make this honest rather than optimistic:

- **Admission and the catalogue come from one probe.** The session that decides a
  candidate is admitted is the session its model list is read from, so a provider
  is never advertised with models no probe produced, and never with a model list
  from an agent that did not answer.
- **The host is the authority, not the control plane.** The server records what
  each machine reported, keyed by host, and dispatch resolves a provider
  *against the host that will run it* — two machines may report the same agent
  name against different executables.

The control plane keeps its own configured provider list as a fallback, which is
what a server with no connected worker answers with. It is not the source of
truth: an agent appears because a machine verified it, and disappears when the
machine stops reporting it.

### Marks

A provider's tab is its icon and nothing else, so an icon that 404s is an
invisible tab rather than a missing decoration, and every offered provider has
one.

Loom follows bb here rather than inventing a scheme. Each agent gets a **drawn
glyph** — the files loom ships are the ones bb's own provider plugins declare
(`plugins/provider-*/icons/`, copied to `crates/server/assets/`) — and every one
of them paints itself with `currentColor`, with tone, where a mark has any,
coming from `fill-opacity`. One file therefore works in both themes: the client
masks the image and fills it with the theme's text colour, so the mark *is* the
theme's ink.

That choice retires the obvious alternative. A vendor's full-colour artwork does
not survive a mask — it flattens to a silhouette — and where the artwork is only
legible because of its colour (`oh-my-pi`'s near-white bars, Cursor's solid
tile) the flattened result is worse than a drawn glyph. Loom also probes agents
bb has no icon for (Gemini); an agent with no glyph of its own is drawn as an
**ACP agent**, which is bb's fallback for the same case.

Colour beyond the theme comes from `strings.iconTint`, and those values are bb's
too: `pi` is `#6D5DFB`, `omp` `#9333EA`, `opencode` `#2563EB`, `claude-code`
`#D97757`, and Cursor carries a real pair (`#111827` light, `#F5F5F5` dark) that
the client resolves with `light-dark()`. The same object carries the agent's own
sign-in command worded into a hint and the vendor's install page, which is what
the client shows when an agent is missing or signed out.

`logoUrl` is content-addressed (`?h=<16 hex of the mark>`). A request naming the
current hash is answered `immutable` for a year; anything else — an old hash, or
none — is answered but never cached, so a client cannot pin a stale icon.

## Decisions taken

| Question | Decision |
| --- | --- |
| One protocol or several? | **ACP only.** Pi is not special-cased at the client. |
| How is Pi reached? | **`pi-acp` embedded as a library**, over `Channel::duplex()`. |
| How are providers declared? | **Discovered at runtime.** A compiled-in known-agent table plus the machine's `PATH`, verified by an ACP handshake. No environment variable, no config key. |
| ACP version | **v2 first, v1 fallback.** The SDK connector selects the highest configured protocol that the agent accepts; v1 remains for stable agents and the default Pi path. |
| Where a displayed conversation comes from | **The agent's session, loaded on demand.** The server keeps no durable transcript; it loads a replay over a dedicated connection and caches it for display. See [`architecture.md`](architecture.md) § The conversation is not in the log. |
| Which host may resume a session | **The one that opened it.** The binding carries a host id, and a dispatch or load on another host starts fresh rather than restoring someone else's session. |
| Resume entry point | **`loom resume <thread>`** — `session/resume` under v2, which replays nothing because continuing a session needs no past; `session/load` under v1, where it is the only restore method. |
| Unsupported capability | **Reported, never worked around.** |
| `CurrentModeUpdate` under v2 | **Skipped.** v2 replaced modes with config options, so v1's mode update has no v2 equivalent and the conversion layer errors on it. Omitting it follows v2's design rather than papering over a gap. |
| Unmapped update type | **Stored and logged, not rendered.** Below the typed layer: on the v2 path `SessionUpdate::Other` carries it; on the v1 path there is no catch-all, so it is intercepted as a raw JSON-RPC frame (`UntypedMessage`) before typed dispatch. Either way the discriminator is logged, and a leading `_` (implementation-private) is distinguished from a future ACP variant. |

## Open questions

- **Does every target agent implement `session/list`?** It is a capability, so
  no. The import flow must handle absence by omission rather than by guessing.
- **Version policy for the crates.** `agent-client-protocol` (the framework) and
  `agent-client-protocol-schema` (the types) version independently, and the
  protocol version under negotiation is a third axis. Loom pins the schema to
  the exact version the framework requires, as `pi-acp` does, so there is one
  schema copy in the tree — type identity matters between the re-exported
  `agent_client_protocol::schema` and a direct dependency.
- **Does the embedded `pi-acp` need its own process isolation?** Running in the
  worker means a panic in the translator takes the worker with it, whereas a
  spawned process would not. Worth deciding explicitly rather than by default.
- **How long to keep the v1 path?** v2 is a draft and may rename things again
  (it already dropped `session/load`). The negotiation makes supporting both
  cheap at the transport level, but the adapter's event mapping has to carry
  both shapes. Deprecate v1 when stable agents stop speaking it, not on a date.

## References

- `agent-client-protocol-schema` 1.5.0
  - `src/lib.rs:44` — the `unstable_protocol_v2` gate; by default `LATEST` is v1
  - `src/v2/conversion.rs` — the bidirectional conversion layer (`try_v2_to_v1`,
    `try_v1_to_v2`); `:1864` is `v1::SessionUpdate`'s v2 form, `:1894` is the
    `CurrentModeUpdate` gap
  - `src/v1/client.rs:99` — v1's `SessionUpdate`, with no `Other` variant
  - `src/v2/client.rs:889` — `AgentMessage` and its patch semantics
  - `src/v1/agent.rs:1171` / `:1498` — `session/load` and `session/resume`
  - `src/v2/agent.rs:1411` / `:1511` — `ResumeSessionRequest` and `ReplayFrom`
- `agent-client-protocol` 2.0.0
  - `src/role/acp.rs:266` — `Client::protocol_connector` (client-side negotiation)
  - `src/role/acp.rs:359` — `AgentProtocolRouter` (agent side)
  - `src/role/acp.rs:828` — `pipe_protocol_peers_until_done`: frames pass through
    unconverted after `initialize`
  - `src/jsonrpc.rs:4491` — `UntypedMessage`, the v1 escape hatch for unknown types
- `pi-acp` (sibling checkout) — `src/agent.rs` (handlers),
  `src/session_store.rs` (id mapping), `src/pi/sessions.rs`
  (`list_pi_sessions`); W-562 shipped v2 support there (feature `protocol-v2`)
- bb `docs/provider-bridge-protocol.md` — narrow grammar and division of labour
- bb `packages/provider-bridge-protocol/src/thread-delta.ts` — the grammar
  itself
- bb `plugins/provider-acp/src/known-agents.ts` — the five ACP agents
- `crates/worker/src/provider.rs` — ACP run metadata and the shared terminal
  event; the old Pi-specific direct driver was removed
- `crates/worker/src/discovery.rs` — the known-agent table and the `PATH`
  lookup that turns it into candidates; the ACP probe in
  `crates/worker/src/acp/catalog.rs` is what admits them
- `docs/provider-sessions-research.md` — the storage survey this builds on
