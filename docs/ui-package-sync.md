## Ported UI packages

The projection and UI primitive package sources were selected from the bb
hard-fork baseline at commit
`fa1f44ebe9e5676004b669e48c99b3c7606466b6` (bb repository commit
`Cut startup JavaScript by 81 KiB and restore 5% bundle headroom (#3476)`).
The local entries are loom's adapted source trees, whose digests are recorded
separately from the upstream trees in `ui/provenance.json`. The package set is:

- `@bb/domain`
- `@bb/server-contract`
- `@bb/thread-view`
- `@bb/client-core`
- `@bb/core-ui`
- `@bb/shared-ui`
- `@bb/desktop-contract`

`thread-view` is used as a pure event-to-timeline projection. `client-core`
contains state and transport helpers only; this issue does not import bb's
application assembly or plugin runtime.

## Source provenance

The source-level pin, app/package/contract hashes, dependency closure, and
product-surface migration matrix are maintained in
[`docs/ui-baseline.md`](ui-baseline.md) and machine-checked by
`scripts/check-ui-provenance.mjs` through [`ui/provenance.json`](../ui/provenance.json).
This document describes the package-level policy; it does not replace the
unified manifest.

loom is a hard fork and intentionally has no `upstream` remote. Future package
updates must be deliberate source comparisons: record the new bb commit here,
compare the corresponding package directories against that read-only checkout,
and port only changes that fit loom's contracts. Do not cherry-pick bb commits
or maintain a patch series.

During a sync, preserve these local constraints:

- providers such as Pi and ACP remain first-class provider IDs, not plugins;
- relay and server contracts remain loom-owned boundaries;
- legacy extension wire types may remain decodable, but must not become
  renderable timeline work rows;
- any behavioral difference needs an explicit test and should live in the
  adapter/input boundary rather than in the projection rules.

The executable projection smoke example is
`ui/examples/thread-timeline.ts`; run it with `pnpm example` after dependencies
are installed.
