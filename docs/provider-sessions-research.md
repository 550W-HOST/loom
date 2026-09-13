# Provider sessions: how each agent stores and resumes a conversation

> **Status: superseded in its conclusions, still valid in its measurements.**
>
> This document surveyed how five agents lay out their session storage. That
> survey has not been repeated and remains the evidence base for loom's
> position that it does not read agent session files.
>
> The design questions it raised, however, have been decided. See
> [`provider-strategy.md`](provider-strategy.md) for the decisions: **loom never
> reads an agent's session files**; every agent is reached through ACP; and
> resume is `loom resume <thread>` backed by `session/load`. Where this document
> presents something as an open question, treat the strategy document as
> authoritative.

Research for W-558 and for the larger question of supporting many agents
(especially ACP) without inventing a storage scheme each one rejects.

The measurements remain useful, but the implementation now follows the ACP
boundary described below rather than reading any of these layouts from loom.
This document exists as the evidence record, not as a second storage design.

## What was measured

### Storage layouts on a real machine

| Agent | Root | Partitioned by | File naming |
| --- | --- | --- | --- |
| Pi | `~/.pi/agent/sessions/` | **working directory**, encoded as `--data-workspace-ljm-dev--` | `<ISO timestamp>_<uuid>.jsonl` |
| OMP | `~/.omp/agent/sessions/` | **working directory**, same encoding as Pi | similar to Pi |
| Claude Code | `~/.claude/projects/` | **working directory**, encoded as `-data-workspace-ljm-dev-lemma` | — |
| Codex | `~/.codex/sessions/` | **date**, `2026/08/09/` | `rollout-<iso>_<uuid>.jsonl` |
| Hermes | `~/.hermes/sessions/` | flat | `request_dump_<time>_<hash>.json` |

Two consequences that shape everything below:

1. **Three of the five partition by working directory.** A session's owning
   directory is encoded in its *path*, not in an index. Moving a session out of
   `~/.pi/agent/sessions/--<cwd>--/` makes it invisible to that agent's own
   lookup.
2. **The partition dimension is not shared.** Codex partitions by date, so there
   is no single `~/.loom/sessions/<agent>/` layout that all five would accept.

### Every session file records its own cwd

Pi writes a header as the first line:

```json
{"type":"session","version":3,"id":"019fe597-a616-7acd-bb00-576804a389f4","timestamp":"2026-08-09T08:15:48.758Z","cwd":"/data/workspace/ljm"}
```

So a session file is self-describing. Its location is a lookup optimization, not
the source of truth.

## ACP already standardises this — and we should use it

`agent-client-protocol-schema` (the crate `pi-acp` already depends on) defines
four session methods:

| Method | Purpose |
| --- | --- |
| `session/new` | Start a session; the agent returns a `sessionId` |
| `session/load` | Restore a stored session by id |
| `session/list` | Enumerate sessions, with an optional `cwd` filter and cursor pagination |
| `session/delete` | Remove a stored session |

`session/list` takes `cwd: Option<AbsolutePath>` — "Filter sessions by working
directory" — which is exactly the partitioning Pi, OMP and Claude do on disk.
The protocol models it instead of leaving it to the client.

The agent advertises support in `initialize`:

```json
{"agentCapabilities": {"loadSession": true, "sessionCapabilities": {"fork": {}}}}
```

**This is the important finding.** Resume is not a filesystem problem in ACP; it
is `session/load <id>`. The agent owns storage, and the client owns the mapping
from its own thread to an agent session id.

### How a real ACP adapter does it

`pi-acp` (already written, in this workspace as a sibling project) implements the
bridge and keeps the mapping itself:

```
<agent dir>/pi-acp/session-map.json
{ "version": 1, "sessions": { "<acp session id>": {
    "cwd": "...", "sessionFile": "...", "additionalDirectories": [...], "updatedAt": "..." } } }
```

- `session/new` spawns pi, records `sessionId -> sessionFile`
- `session/load` looks up `sessionFile` and starts pi with `--session <file>`
- `session/list` walks `~/.pi/agent/sessions/`, reads each file's header for
  `cwd`, and filters by the requested directory

So the pattern that already works is: **agent owns storage; the adapter owns the
id mapping and knows how to enumerate the agent's storage.**

### bb reached the same conclusion

`packages/provider-bridge-acp/src/wire.ts` declares `loadSession` and
`sessionCapabilities`, and `provider-pi/src/bridge/bridge.ts` implements
`thread/resume`, including a guard for the case where a resumed session's
original cwd no longer exists:

> `Cannot resume: the pi session's working directory "<path>" no longer exists.`

That guard exists because cwd is part of a session's identity. It is a real
failure mode worth copying.

## What this means for loom

### The implementation now in use

```
loom Thread.provider_session_id + provider_session_binding
  → RunDispatch.provider_session_id (only when agent and cwd match)
  → ACP session/load <id, cwd>
  → agent-owned session storage
```

The old `effective_argv` / `--session-dir` / `--session-id` path was removed;
loom never scans or opens a provider session file.

### What was implemented, as of W-566

| Piece | Where | State |
| --- | --- | --- |
| `thread -> (agent, session id, cwd)` | `loom_domain::ProviderSessionBinding`, on the thread | stored, replayed, and checked at dispatch |
| the mismatch guard | `Thread::resumable_session_id` | agent or workspace change starts fresh |
| the missing-cwd guard | `crate::acp::session::drive` | explicit failure naming the path |
| `session/list` | `crate::acp::sessions::list_sessions` | capability-gated; `Unsupported` is distinct from an empty list |
| capability probe | same module, `initialize` | `load_session` and `list_sessions`, never inferred |

The import **surface** is the daemon's `list_sessions` returning
`SessionListOutcome`. The control plane has no route for it yet: the exported bb
contract declares no session-import endpoint (`grep -c session` over
`contracts/bb/server-api.json`'s route ids is 0), so there is nothing to conform
to and no client call to serve. The capability gate and the outcome distinction
were implemented because they are the part the acceptance criteria name — "按
capability 明确省略，不扫描 agent 私有文件格式" — and they are testable without
a route. Adding a non-contract route would grow loom's own API surface, which
`docs/api-coverage.md` exists to prevent.

### The three questions this posed, and how they were decided

**1. Where do sessions live?** — *Decided: adapter-owned mapping; storage
stays wherever the agent wants.* loom stores only
`thread -> (agent, session id, cwd)` in its domain snapshot and never chooses a
session directory. The first two options below were rejected: agent-native roots
make loom's sessions indistinguishable from hand-run ones and give no uniform
identity, while `~/.loom/sessions/<agent>/` asks agents to relocate their
storage, which the measured layouts show they will not all do.

| Option | Outcome |
| --- | --- |
| Agent-native roots (`~/.pi/agent/sessions/...`) | Rejected |
| `~/.loom/sessions/<agent>/` | Rejected |
| Adapter-owned mapping, storage wherever the agent wants | **Adopted** |

**2. What is the user-facing entry point?** — *Decided: an agent-agnostic
`loom resume <thread-id>`.* The per-agent wrapper was rejected because it
multiplies commands as agents are added. `session/load` covers ACP agents, and
ACP is now the only path, so one command covers everything.

**3. Does loom need sessions to be *files*?** — *Decided: no.* This was the
question that settled the design. With ACP as the single path, there is no file
for loom to find, so any scheme built on symlinking or hardlinking is dead on
arrival. The id mapping plus `session/list` / `session/load` is the whole
mechanism.

### A design that survives both kinds of agent

The survey produced this shape:

```
loom thread  ──owns──▶  (agent, session id, cwd)   stored in the domain snapshot
                              │
       file-partitioned agent │ ACP agent
       (~/.pi, ~/.claude)     │ (omp, hermes, …)
                              │
   locate file by id/cwd      │ session/load <id>
   (walk the root, read       │
    each header's cwd)        │
```

Its lasting contribution is the layer it separates:

- The mapping belongs in loom's domain state, next to the thread — it is
  per-thread metadata, and W-543 already persists that.
- Enumeration and resume are **adapter** concerns, because only the adapter
  knows how its agent addresses sessions.
- The entry point is a single agent-agnostic `loom resume <thread-id>`.

The left branch was then deleted rather than implemented. Once every agent goes
through ACP, no adapter is file-partitioned, so `loom` never walks a session
directory. The right branch is the whole design:

```
loom thread  ──owns──▶  (agent, session id, cwd)   stored in the domain snapshot
                              │
                              │ ACP adapter
                              │
                        session/load <id>
```

The file-reading code lives in `pi-acp`, which already does it for Zed. loom
reuses the adapter rather than duplicating the format knowledge — which is why
this branch survived and the other did not.

## Open questions

None of these are open any longer as *design* questions; they are kept as the
record of what the survey could not settle, with how each was resolved.

- **Does every ACP agent implement `session/list`?** — It is a capability, so
  an agent may not. Still true, and now a product rule rather than a gap: an
  agent that cannot list simply does not appear in the import list.
  `provider-strategy.md` records this under "No fallbacks".
- **Does `--session-id <loom thread id>` conflict with Pi's own id format?** —
  *Resolved by removal.* The `--session-id` rewriting is part of the Pi-specific
  path that the ACP migration deletes (`effective_argv` in
  `crates/daemon/src/provider.rs`). With `pi-acp` owning session identity, loom
  no longer passes a thread id where a Pi UUIDv7 is expected, so the conflict
  cannot arise. The inconclusive probe no longer blocks anything.
- **What is the right behaviour when a resumed session's cwd is gone?** —
  *Decided*: refuse with an explicit error quoting the missing path. Never
  silently start a fresh session. See "No fallbacks" in `provider-strategy.md`.
- **Should loom adopt Pi through ACP instead of JSON-RPC?** — *Decided: yes.*
  `pi-acp` is embedded as a library, so the "cost of a bridge process" that
  this question weighed no longer applies. See "Pi is embedded, not spawned".

## References

- `agent-client-protocol-schema` — `ListSessionsRequest.cwd`,
  `agentCapabilities.loadSession`
- `pi-acp` (sibling checkout) — `src/session_store.rs`,
  `src/pi/sessions.rs::list_pi_sessions`, `src/agent.rs` handlers
- bb `packages/provider-bridge-acp/src/wire.ts` — capability declaration
- bb `plugins/provider-pi/src/bridge/bridge.ts` — `thread/resume` and the
  missing-cwd guard
- `crates/daemon/src/provider.rs` — loom's current Pi-only session arguments
