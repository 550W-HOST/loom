## Ported UI packages

The projection and UI primitive package sources were taken once from the bb
hard-fork baseline at commit
`fa1f44ebe9e5676004b669e48c99b3c7606466b6` (bb repository commit
`Cut startup JavaScript by 81 KiB and restore 5% bundle headroom (#3476)`).
Fourteen workspace packages live under `ui/packages/`, with the disposition the
port recorded for each one:

- `@bb/domain` — adapted source
- `@bb/server-contract` — adapted source
- `@bb/thread-view` — adapted source
- `@bb/client-core` — adapted source
- `@bb/core-ui` — adapted source
- `@bb/shared-ui` — adapted source
- `@bb/desktop-contract` — adapted source
- `@bb/config` — adapted source
- `@bb/host-daemon-contract` — adapted source
- `@bb/sdk` — adapted source
- `bb-plugin-automations` — adapted source
- `@bb/mobile-bridge` — exact copy of the bb source
- `@bb/fuzzy-match` — exact copy of the bb source
- `@bb/tsconfig` — exact copy of the bb build configuration

These are ordinary first-party workspace packages now: nothing pins their bytes
to bb, and a change to one of them is a normal commit reviewed on its own diff.
The commit above stays the recorded answer to "where did this source come from".

`thread-view` is used as a pure event-to-timeline projection. `client-core`
contains state and transport helpers only; this issue does not import bb's
application assembly or plugin runtime.

## Sync policy

The pin lives in [`contracts/bb/manifest.json`](../contracts/bb/manifest.json),
which the contract job uses to re-export `contracts/bb` byte-for-byte. The
product-surface decisions are recorded in
[`docs/ui-baseline.md`](ui-baseline.md); this document describes the
package-level policy.

loom is a hard fork and intentionally has no `upstream` remote. Future updates
must be deliberate source comparisons: check out the target bb commit read-only,
compare the corresponding package directories, and port only changes that fit
loom's contracts. Record the new commit here when one is taken. Do not
cherry-pick bb commits or maintain a patch series.

During a sync, preserve these local constraints:

- providers such as Pi and ACP remain first-class provider IDs, not plugins;
- relay and server contracts remain loom-owned boundaries;
- legacy extension wire types may remain decodable, but must not become
  renderable timeline work rows;
- any behavioral difference needs an explicit test and should live in the
  adapter/input boundary rather than in the projection rules.

## Recorded drifts

A behavioural difference from the bb baseline is recorded here rather than left
to be rediscovered during the next sync.

### `client-core` timeline merge — `generation`

`ui/packages/client-core/src/timeline/timeline-merge.ts` and its test carry a
field bb does not have. loom's timeline response numbers rows per
`generation` (a rebuild renumbers from one, and a generation is never reused),
and `LoadedTimelineState` holds it: a page from another generation replaces the
rows instead of being merged with them, because sequences from two numberings
are not comparable. bb's client has no server-side rebuild, so it has no such
field and merges purely by sequence.

Two smaller related drifts, in the same change:

- `@bb/server-contract`'s timeline response now declares `generation` and
  `history { status, complete, reason }`, and a row's `startedAt`/`createdAt`
  are nullable. bb's rows always carry both times; loom's replayed rows do not
  know one.
- `@bb/thread-view` renders a duration only when a start is known
  (`timeline-row-title.ts`), and a work summary's time is unknown when any child
  is. bb can assume every row has a time.

A sync that takes a newer bb revision of either file must keep these: they
encode a server contract, not a local preference.

## Verification

There is no standalone smoke target for the projection — `ui/examples` and the
`pnpm example` script no longer exist — and no hash manifest to refresh. What
keeps the package set honest is the suites and the type check:

```bash
pnpm --filter './ui/packages/*' run test   # each package's own suite
pnpm --filter @bb/app run test             # the app against these packages
pnpm --filter @bb/app run typecheck
```
