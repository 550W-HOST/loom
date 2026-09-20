# Domain-state persistence

How the control plane's **entity view** — projects, threads, hosts and
environments — survives a restart, without turning the relay into a second
database and without an external service.

## The problem

With `DiskBackend` (`--data-dir`), the relay log already survives a restart:
a client can replay a thread's whole timeline. But `DomainRegistry` and
`RunRegistry` were pure in-memory maps, so after a restart the server no longer
recognised the thread those events were about. The list was empty, a message
was rejected as "thread not known", and the data was present but unreachable.
That is more confusing than data loss, because there is no way to tell from the
outside that the entity view is what went missing.

## The decision: snapshot baseline + log delta

We use a **hybrid**: a periodic (and shutdown-time) **snapshot** of the entity
view carries a **watermark** — the newest relay `EventId` the snapshot
incorporates — and recovery loads the snapshot, then replays the retained log
events **after** that watermark. This was chosen over the two pure options:

| Option | Why not |
| --- | --- |
| **(a) snapshot only**, write periodically and on shutdown | A crash between snapshots silently drops every mutation since the last one. The snapshot is not a log; there is nothing to bring it forward. |
| **(b) rebuild purely from the log** | Retention trims oldest-first. An old project's or thread's `*_created` event is gone long before the entity stops mattering, so pure replay cannot reconstruct entities older than the window. It also cannot recover the **personal project**, which is seeded in memory and never publishes an event. |
| **(c) snapshot + log delta** (chosen) | The snapshot is the baseline that outlives retention; the retained log is the delta since it. It meets consistency and performance without an external database. |

The log remains the source of truth for *what happened*. The snapshot only
answers *what exists now*, and it is a compact projection, not a copy of the
log: messages and run-event history are deliberately absent (see below).

## What is stored — and what is not

`domain.snapshot` in the data directory holds a `DomainSnapshot`:

```text
DomainSnapshot
  version        framing/format version
  watermark      Option<EventId> — newest event the view incorporates
  registry       RegistrySnapshot { personal_project_id, projects, threads,
                                    hosts, environments, queued_messages,
                                    interactions, thread_sections }
  runs           in-flight RunRecord list
  settings       SettingsSnapshot { appearance, experiments, general,
                                    keyboard, ui_preferences }
  automations    AutomationState { version, automations, runs, thread_marks }
```

One atomic write covers both the entity view and its watermark, which is the
"atomic commit of both" the design note worried about.

`queued_messages` and `interactions` (batch B3) and `thread_sections` (batch B7)
are `#[serde(default)]`, which is the whole compatibility story for this file: a
snapshot written by a build that predates them still loads, with an empty queue,
no pending interaction and no sections — precisely the view that build would
have held. Bumping `SNAPSHOT_VERSION` for an additive field would force every
deployment to discard a recoverable snapshot.

Two project fields added in B7 are also `#[serde(default)]`: `deleted_at_ms`,
the tombstone that keeps a deleted project from being resurrected by replay, and
`sort_key`, the client's explicit rank. A project from an older snapshot loads
with neither, which sorts it by creation time exactly as that build did.

`settings` is an additive B10 field. A snapshot written before B10 receives
the current server-local defaults on restore; its settings payload has its own
version so additive preference keys can be migrated without bumping the outer
snapshot format. UI preference writes use one mutex-protected
`expectedRevision` check and increment, then synchronously update the same
snapshot file. They are not relay events and never belong to a thread or
provider session.

`automations` is additive in the same way, and for the same reason: an
automation describes a schedule and a target, not a thread or a run in the log,
so replaying the log over it would be meaningless. Its payload version is what
carries the scheduler's own migration: a version-1 payload stored run rows as
`running` because nothing could claim one, and restoring it rewrites them to
`pending` — the queue entries they always were — while the sweep arms the
schedules that release had no `nextRunAt` for. The payload carries its own
version; its rows are stored as data and decoded per row, so a row written by
another build is reported (`invalid-stored-data`) rather than failing the
restore, and a snapshot from a build that predates the field restores exactly
as that build would have described the workspace. Automations are written
synchronously after each mutation, the way a settings write is. See
[`automations.md`](automations.md).

Both sets are stored **in every status**, not only the open ones. A sent queued
message and a resolved interaction are part of what a client renders (a retry
row, an answered approval) and its `updatedAt`/`resolvedAt` are what a render
sorts on, so they are entity-view rows rather than a work queue that empties.
They are bounded by use, and a deployment that needs an age-based trim wants
that policy explicit rather than implied by deletion.

The automation scheduler writes through the same file. A sweep that queues a run
writes the snapshot before the run can be observed, so the window it claimed is
behind the automation's `nextRunAt` on disk as well as in memory: a restart
cannot find that window due a second time. A run that was `running` when the
process stopped is failed on the next start, exactly like a provider run, and
its automation takes the failure through the ordinary retry policy (see
[`automations.md`](automations.md)).

**Not stored, on purpose:**

- thread messages — the registry holds no timeline; the log is where a
  conversation lives and replay returns it byte-identically;
- run-event history — same reason;
- the relay log itself — that is `DiskBackend`'s job.

So there is no duplicate storage of the log. The relationship is: the log is
append-only history; the snapshot is the derived, bounded entity view plus its
position in that history. Together they are a recovery point; neither is
redundant.

## Why the watermark is consistent

The snapshot writer reads the watermark **first**, then copies the entity view.
That ordering is the whole argument:

1. every command mutates the registry *before* it publishes the event, so an
   event that is already in the log at or below the watermark had its mutation
   applied before the watermark was read;
2. the entity view is therefore copied *after* every event the watermark
   covers — it can only be **ahead** of the watermark, never behind;
3. a mutation that raced ahead of the watermark has an event id above it, so
   recovery replays it — and replay is idempotent, so applying it to a view
   that already has it is harmless.

The dangerous direction (a watermark that includes an event whose mutation is
*missing* from the view) cannot occur under that ordering.

Replay itself is tolerant by construction: `DomainRegistry::apply_event` upserts
entities by id, sets statuses directly (never through the transition table, so a
replayed sequence cannot fail), and **ignores** an event whose entity is
unknown. A log entry for a thread the snapshot does not have is a no-op, not a
panic — that is the "log has events but the entity is missing" case.

Replay is applied per shard, in append order. All of an entity's events share
its scope's shard, so per-entity order is preserved without a global merge;
cross-entity order never matters to the entity view.

## Crash safety

Writing a snapshot is `create temp → write → fsync → rename → fsync
directory`. `rename` is atomic, so a reader sees either the complete previous
snapshot or the complete new one — never half of one. The framed envelope
(8-byte magic, format version, payload length, CRC-32) is a second line of
defence: a truncated or bit-rotted file is detected and rejected rather than
deserialized into a plausible-looking but wrong view. The worst case is a
roll-back to the previous consistent point; the log delta since it is replayed
on top.

For the same reason `AppState::build` never fails on a bad snapshot. A snapshot
that cannot be trusted is treated as **absent** and the retained log is replayed
from the beginning; refusing to start is a worse outcome than starting from an
older, consistent view.

## In-flight runs after a restart

**Decision: every run that was in flight when the process stopped is failed,
with a terminal `thread_run_event` (`RunOutcome::Failed`) and a
`thread_status_changed` out of `working`.**

Reasoning:

- the control plane cannot prove a provider is still running after a restart,
  so declaring the run successful or leaving it open would be a guess;
- the invariant the server owns is *"no thread is stuck in `working`, and every
  run ends in exactly one terminal event"*. Failing is the only outcome that
  preserves it;
- a worker that did keep running and later reports a terminal event is handled
  idempotently: the run is no longer in the table, so the report is an accepted
  no-op (`ReportOutcome::Unknown`).

Recovery covers both cases: runs captured in the snapshot (terminated using
their recorded run id and thread) and threads still marked `working`/`waiting`
whose run was dispatched after the last snapshot (terminated using the thread's
recorded `active_run_id`). The two passes cannot double-report — a thread
already moved out of `working` is skipped.

**The same rule covers automation runs, with one difference.** An automation run
that was `running` when the process stopped is failed with "the server restarted
while this run was in flight", and its automation takes that failure through the
ordinary retry policy — otherwise single-flight would block the automation
behind a run nobody will ever finish. A run that was still **`pending`** is not
touched: it is durable work that has not started, so it survives and the sweep
simply does not claim a second one while it waits.

## Configuration and operation

- **Enabled** when `--data-dir` (`AppConfig::backend_path`) names a data
  directory; the snapshot lives beside the shard files.
- **Settings scope**: appearance, experiments, general/keyboard settings and
  UI preferences are server-local and shared by clients of that server. They
  survive restart when `--data-dir` is configured. The zero-configuration
  in-process backend remains intentionally ephemeral, including its settings.
- **Periodic write**: `AppConfig::snapshot_interval`, default 30 s.
  `Duration::ZERO` disables the background writer.
- **Shutdown write**: `Ctrl-C` drains connections and then writes a final
  snapshot. A hard kill skips it; the periodic writer and log replay are what
  make that safe.
- **In-process and Redis backends**: no local entity-view persistence. The
  in-process backend has no durability at all; the Redis log is shared across
  nodes, so a per-node snapshot would be ambiguous. Domain state there is
  ephemeral, which is unchanged from before this work.
- **Retention is untouched.** `replay_grace` / `trim_horizon` / `ttl` and the
  per-shard cap keep exactly the semantics they had. The snapshot only decides
  how far back replay *needs* to look.
- **Host status** is restored as recorded. No worker can be attached to a
  process that just started, so the first reconciliation pass marks a host that
  is not heartbeating as disconnected and reaps its runs; a worker that
  reconnects re-enrols under its existing id and becomes connected again.

If the per-shard cap evicts events newer than a snapshot's watermark (a very
large burst between snapshots), the gap is detected and reported; the view is
then stale, never wrong, and a fresh snapshot re-establishes the baseline.

## Tests

- `persistence::tests` — round trip, atomic replacement with no staging file
  left behind, truncated body rejected, bit flip caught by the checksum, missing
  directory reads as "no snapshot".
- `domain_state::tests` — export/restore round trip, replay of an event whose
  entity is missing is a no-op, replay is idempotent.
- `state::tests` — write → shutdown → reopen preserves projects, threads,
  environments and hosts with unchanged ids; an in-flight run is failed and the
  last status change on the wire agrees with the restored status; a log without a
  snapshot is rebuilt from it; a corrupt snapshot falls back to the log.
- `automations::tests` — a row round-trips through its stored form, a row
  missing an additive field still reads, a row whose stored discriminator
  contradicts its JSON (or that parses at all) is reported as a read problem, a
  newer payload version is left alone, an older one is migrated in place, and
  duplicate ids collapse deterministically.
- `automations::tests` (scheduler) — a due window is claimed once and the
  schedule moves past *now*, a run in flight holds the next window back, pause
  cancels queued runs and resume does not replay the paused window, the retry
  policy and the third-failure pause, and a restart that fails interrupted runs
  while keeping queued ones.
- `tests/automations_conformance.rs` — automations survive a durable restart,
  a snapshot written before the field existed still loads, and a hand-damaged
  payload is listed as problems rather than dropping rows.
