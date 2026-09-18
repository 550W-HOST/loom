# bb contract export

Read-only exporter that turns bb's TypeScript/zod contract packages into
language-neutral JSON Schema artifacts under `contracts/bb/`.

`loom-server` has to speak bb's HTTP and WebSocket contract so bb's UI and
execution plane can attach unchanged. Reading 10k lines of TypeScript to learn
that contract is not a plan, so this tool extracts it and the Rust
`loom-contract` crate turns it into assertions.

## What it exports

| Artifact | Source | Contents |
| --- | --- | --- |
| `server-api.json` | `packages/server-contract/src/public-api.ts` + `api/*` | 167 HTTP routes: path, method, request JSON Schema (zod), response JSON Schema (resolved TypeScript type) |
| `client-ws.json` | `packages/domain/src/change-kinds.ts`, `api/terminals.ts` | UI and terminal WebSocket messages, subscription targets, change kinds |
| `host-daemon.json` | `packages/host-daemon-contract/src/*` | worker commands, results, WebSocket messages, enrollment/session/event HTTP shapes |
| `error-codes.json` | `apps/server/src/**` throw sites | error code -> HTTP status inventory |
| `thread-event.json` | `packages/domain/src/provider-event.ts` | complete `ThreadEvent` union and schemas indexed by `type` |
| `manifest.json` | — | format version, bb revision, counts, file hashes |

## How it works

1. **Scratch module tree.** bb's packages resolve `@bb/*` through pnpm workspace
   symlinks that a plain `git clone` does not have. The exporter copies the
   contract packages into `tools/contract-export/.work/node_modules/@bb/*` and
   drops in the pinned `zod`/`hono` runtime, so no install happens inside bb.
2. **Runtime load.** `bun` imports `publicApiRoutes` and the worker/WS schemas
   directly. Request bodies are converted with `z.toJSONSchema` — exact, not
   inferred.
3. **Type resolution.** Response shapes are types, not runtime values. The
   exporter generates a file of `PublicApiSchema[path][method]["output"]`
   aliases and lowers each resolved type to JSON Schema with the TypeScript
   compiler. Indexing the contract's own type means a response cannot drift
   from what TypeScript clients see.
4. **Interning.** Repeated subtrees are replaced with `#/$defs/<name>` refs,
   restricted to nodes that carry JSON Schema keywords so route/response
   descriptors stay real objects for typed consumers.

## Run it

```bash
BB_SRC=/path/to/bb scripts/export-bb-contract.sh
# or
scripts/export-bb-contract.sh /path/to/bb
```

Requires `bun`. The first run installs the exporter's pinned dependencies.

## Extending it

- **A new bb route** needs no change — `publicApiRoutes` is walked wholesale.
- **A new WebSocket surface** (another protocol file): add it to
  `collectProtocols` in `src/collect.ts` and pick the message schema names
  there.
- **A renamed bb export** fails the export loudly (`schemaFrom` throws), which
  is intentional: a silent drop would make the Rust side conform to a contract
  that no longer exists.
- **A type the TS converter cannot lower** is reported in the manifest under
  `failures.opaqueResponseTypes` rather than emitted as valid-looking `{}`.

See `docs/contract.md` for the contract-level decisions and how to add a
conformance test.
