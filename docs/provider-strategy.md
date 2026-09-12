# Provider strategy: one ACP path

Decision record. loom standardises every agent on ACP. Pi is not special-cased.

## What was decided

1. **All agents are reached through ACP.** One adapter, N agents.
2. **`loom resume <thread>` is the only resume entry point.**
3. **No fallback paths.** An agent that cannot do something is reported, not
   worked around.

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

| Method | Purpose |
| --- | --- |
| `session/new` | Create; the agent returns a `sessionId` |
| `session/load` | Restore by id |
| `session/list` | Enumerate, with `cwd` filter and cursor pagination |
| `session/delete` | Remove |
| `session/set_mode`, `session/set_config_option` | Session configuration |

Capabilities are advertised in `initialize`:
`agentCapabilities.loadSession`, `agentCapabilities.sessionCapabilities.fork`.

### Streaming and timeline

`SessionUpdate` (v2) covers the whole render surface:

```
UserMessageChunk / UserMessage
AgentMessageChunk / AgentMessage
AgentThoughtChunk / AgentThought
ToolCallUpdate / ToolCallContentChunk
TerminalUpdate / TerminalOutputChunk
PlanUpdate / PlanRemoved
StateUpdate
UsageUpdate
SessionInfoUpdate
AvailableCommandsUpdate / ConfigOptionUpdate
```

Two properties matter for loom's timeline:

- **Chunked variants stream.** `AgentMessageChunk`, `AgentThoughtChunk`,
  `ToolCallContentChunk`, `TerminalOutputChunk` are deltas, so text arrives
  incrementally rather than as whole messages.
- **The non-chunked variants are full objects with ids, and repeat updates patch
  by id.** The schema documents this: *"When a client receives another
  `agent_message` update with the same `messageId`, fields in the new update
  patch the previous fields for that message."*

The second is what makes an event log work: an item's identity is stable across
updates, so a log can carry patches and a consumer can converge.

### What v2 adds over v1

`StateUpdate`, `AgentMessage`/`AgentThought`/`UserMessage` (the patchable full
objects), `TerminalUpdate`/`TerminalOutputChunk`, and `Other` for forward
compatibility. loom should target v2.

## The resulting loom architecture

```
loom daemon (one process)
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
daemon as a library** rather than run as another process.

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
`impl ConnectTo<Client>` so a caller can supply a channel instead of stdio.
`run()` keeps its signature and behaviour, so Zed is unaffected.

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

```
loom resume <thread-id>
  → look up (agent, session_id, cwd) in domain state
  → start the agent
  → session/load { sessionId, cwd }
  → the agent restores its own storage
```

**loom never reads an agent's session files.** It asks the agent. This is the
property that makes the design clean: no format parsing, no layout assumptions,
no per-agent storage code.

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
| Agent does not advertise `loadSession` | `loom resume` reports it is unsupported. No copy-a-file fallback. |
| Agent does not advertise `session/list` | The agent does not appear in the import list. loom does not scan a guessed directory. |
| A resumed session's `cwd` no longer exists | Explicit error. bb's wording is a good model: *"Cannot resume: the session's working directory `<path>` no longer exists."* Never silently start a fresh session. |
| Agent process dies mid-turn | The turn ends in a terminal state with the reason. No automatic replay into a new session. |
| `SessionUpdate::Other` | Logged as unmapped, not coerced into a nearby type. |

The last row is the same rule W-538 applied to `provider/unhandled`: an
unmapped frame is reported, never given a catch-all body.

## Migration

Current state: loom has Pi-only JSON-RPC (`crates/daemon/src/provider.rs`,
`effective_argv`) and a generic `custom` spec. `crates/provider-protocol`
carries `ProviderSpec`, `RunDispatch`, `ProviderReport`, and the stdout guard.

Required changes, in dependency order:

1. **Define the ACP-adapter boundary** in `loom-domain` / `provider-protocol`:
   what an event is, how the adapter reports it, and how `(agent, session_id,
   cwd)` is recorded.
2. **Build loom's ACP client** and wire two kinds of peer to it: an embedded
   `pi-acp` (requires the transport-injection entry point tracked in the
   `pi-acp` project) and a spawned native ACP agent. The client code is
   identical for both.
3. **Remove the Pi-specific path** — `effective_argv`'s `--session-dir` /
   `--session-id` rewriting, and the `pi` special case in `ProviderSpec`.
4. **Add `loom resume <thread>`** and the import flow on top of
   `session/load` / `session/list`.
5. **Decide the daemon's user model.** With no file reading, the loopback
   argument for a user-level service weakens to "resource isolation versus
   convenience" and becomes independent of sessions (was W-558).

## Decisions taken

| Question | Decision |
| --- | --- |
| One protocol or several? | **ACP only.** Pi is not special-cased at the client. |
| How is Pi reached? | **`pi-acp` embedded as a library**, over `Channel::duplex()`. |
| ACP version | **v2.** v1 is refused rather than degraded. |
| Resume entry point | **`loom resume <thread>`**, backed by `session/load`. |
| Unsupported capability | **Reported, never worked around.** |
| `SessionUpdate::Other` | **Stored and logged, not rendered.** The schema requires preserving the payload; rendering an unknown structure carries no meaning. The discriminator is logged, and a leading `_` (implementation-private) is distinguished from a future ACP variant. |

## Open questions

- **Does every target agent implement `session/list`?** It is a capability, so
  no. The import flow must handle absence by omission rather than by guessing.
- **Version policy for `agent-client-protocol`.** Pinned? The protocol version
  (v2) and the crate version are different axes.
- **Does the embedded `pi-acp` need its own process isolation?** Running in the
  daemon means a panic in the translator takes the daemon with it, whereas a
  spawned process would not. Worth deciding explicitly rather than by default.

## References

- `agent-client-protocol-schema` 1.5.0 — `src/v2/client.rs` (`SessionUpdate`),
  `src/v2/agent.rs` (`ListSessionsRequest.cwd`, `LoadSessionRequest`)
- `pi-acp` (sibling checkout) — `src/agent.rs` (handlers),
  `src/session_store.rs` (id mapping), `src/pi/sessions.rs`
  (`list_pi_sessions`)
- bb `docs/provider-bridge-protocol.md` — narrow grammar and division of labour
- bb `packages/provider-bridge-protocol/src/thread-delta.ts` — the grammar
  itself
- bb `plugins/provider-acp/src/known-agents.ts` — the five ACP agents
- `crates/daemon/src/provider.rs` — loom's current Pi-only path
- `docs/provider-sessions-research.md` — the storage survey this builds on
