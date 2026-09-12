# Provider sessions: how each agent stores and resumes a conversation

Research for W-558 and for the larger question of supporting many agents
(especially ACP) without inventing a storage scheme each one rejects.

Nothing here is implemented yet. This document exists so the design can be
decided from measured facts rather than from an assumption about what a
"session directory" means — because it means something different per agent.

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

### The current state

```
crates/daemon/src/provider.rs   effective_argv()
  if spec.name == "pi" and session_dir is set:
      add --session-dir <dir> --session-id <thread_id>
```

- loom has **no ACP support at all** (only Pi and a generic `custom`)
- the daemon currently runs as a system user (`loom`), so its `$HOME` is
  `/var/lib/loom` and none of the user's `~/.pi` is reachable (W-558)

### The three questions to decide

**1. Where do sessions live?**

| Option | Consequence |
| --- | --- |
| Agent-native roots (`~/.pi/agent/sessions/...`) | Native `pi --resume` works with zero extra machinery. loom does not control the layout, and its sessions mix with hand-run ones. |
| `~/.loom/sessions/<agent>/` | loom controls layout and can map ids uniformly. Requires each agent to support redirecting its storage, which Codex (date-partitioned) and Hermes (flat) may not. |
| Adapter-owned mapping, storage wherever the agent wants | loom stores only `thread -> (agent, session id, cwd)`. Works for every agent including ACP, where there is no file to move at all. |

**2. What is the user-facing entry point?**

`loom pi --resume` (a per-agent wrapper) versus an agent-agnostic
`loom resume <thread-id>`. The second can be implemented for file-partitioned
agents by locating the file, and for ACP agents by `session/load`, so one
command works across both.

**3. Does loom need sessions to be *files*?**

For ACP agents, no — the session never appears on disk in a form loom chose.
Any design that depends on symlinking or hardlinking a directory will not cover
them. A design that stores an id mapping and asks the agent (`session/list`,
`session/load`) covers both kinds.

### A design that survives both kinds of agent

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

- The mapping belongs in loom's domain state, next to the thread — it is
  per-thread metadata, and W-543 already persists that.
- Enumeration and resume are **per-agent adapter** concerns, because only the
  adapter knows whether its agent is file-partitioned or id-addressed.
- `loom resume <thread-id>` is the single entry point; `loom pi` exists only if
  a user wants to drop into the raw CLI.

This is also what makes the ACP path cheap: an ACP provider needs no storage
code in loom, only `session/new` / `session/load` plumbing and the id mapping.

## Open questions

- **Does every ACP agent implement `session/list`?** It is a capability, so an
  agent may not. The mapping in loom's domain state is the fallback: loom
  already knows the session id it used, so resume does not depend on listing.
- **Does `--session-id <loom thread id>` conflict with Pi's own id format?**
  (Pi uses UUIDv7; loom passes `thr_...`.) An earlier probe did not produce a
  session file, but the probe closed stdin before `agent_settled`, so the test
  was inconclusive. This must be measured properly before relying on it.
- **What is the right behaviour when a resumed session's cwd is gone?** bb
  refuses with a clear error. loom should decide explicitly rather than
  silently starting a fresh session.
- **Should loom adopt Pi through ACP instead of JSON-RPC?** `pi-acp` already
  exists and speaks ACP; using it would give one provider path for Pi *and*
  every other ACP agent, at the cost of a bridge process. Worth deciding before
  building more Pi-specific code.

## References

- `agent-client-protocol-schema` — `ListSessionsRequest.cwd`,
  `agentCapabilities.loadSession`
- `pi-acp` (sibling checkout) — `src/session_store.rs`,
  `src/pi/sessions.rs::list_pi_sessions`, `src/agent.rs` handlers
- bb `packages/provider-bridge-acp/src/wire.ts` — capability declaration
- bb `plugins/provider-pi/src/bridge/bridge.ts` — `thread/resume` and the
  missing-cwd guard
- `crates/daemon/src/provider.rs` — loom's current Pi-only session arguments
