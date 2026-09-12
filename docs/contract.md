# The bb contract and loom

loom reuses bb's UI and execution plane, so `loom-server` has to speak the HTTP
and WebSocket contract bb already defines. Reading bb's TypeScript to learn that
contract does not scale and cannot be enforced, so this document describes the
machine-readable export under `contracts/bb/` and the decision it encodes.

## Decision: loom's public surface is bb's contract, not a variant

There are two protocol surfaces in loom, and they have different rules.

**1. UI-facing surface — adopt bb's contract wholesale (subset, no additions).**
The plan is to serve bb's UI bundle unchanged (`LOOM_UI_DIR`), and bb's client
code is the consumer. A UI cannot negotiate a dialect, so anything the UI can
see must be byte-compatible with bb. That covers:

- `/api/v1/*` HTTP routes, shapes and error bodies,
- `/ws` client messages (`subscribe`/`unsubscribe`/`ping` -> `changed`/`pong`),
- `/ws/terminals/:terminalId` terminal messages.

loom-specific frames must never be sent to a bb client. The contract is a lower
bound, not a suggestion.

**2. Server↔daemon surface — intentional divergence.** bb's daemon protocol is
replaced because the daemon is a Rust rewrite (`loom-daemon`) and the relay
owns delivery. The relay envelope in `crates/server/src/protocol.rs`
(`subscribe { scope }`, `event { event_id, scope, payload }`, `enroll_host`,
`run_report`, ...) is the **internal transport**, not the client protocol. It is
still captured here as `host-daemon.json` so the Node execution plane can be
checked in against it later, but loom is allowed to differ. This is the
"明确分歧" the issue asked for: the divergence is real and is confined to the
daemon half of the wire.

### What this changes for the current code

`crates/server/src/protocol.rs` currently serves both audiences on one `/ws`.
That is the thing to split, and it is follow-up work (this issue is export
tooling only):

- `/ws` becomes bb's client protocol. loom's `welcome`, `subscribed`,
  `event`, `host_enrolled` and friends move off it.
- daemon traffic moves to a daemon-only endpoint (bb uses `/internal/ws`).
- `/api/v1/*` is reserved for bb's routes. loom-native control endpoints
  (`/api/v1/publish`, `/api/v1/replay`, the current `/api/v1/version`) must move
  under a distinct prefix so they cannot collide with a route bb's UI expects.

Until that split lands, loom's `/ws` and `/api/v1/*` are knowingly divergent and
the contract tests will not cover them.

## Artifact format: JSON Schema, not OpenAPI

The export is **JSON Schema 2020-12 plus a manifest**, one file per surface.

Why not OpenAPI:

- Two of the four surfaces are not HTTP. bb's UI `/ws`, terminal `/ws` and the
  daemon `/ws` are WebSocket message protocols. OpenAPI cannot express them, so
  an OpenAPI document would cover at most a third of the contract and hide the
  rest.
- The contract is generated from zod and TypeScript, not spec-first. zod v4
  emits JSON Schema 2020-12 natively; wrapping that in OpenAPI adds a lossy
  layer with no consumer.
- The conformance target is a Rust server that must match *shapes*. JSON Schema
  is directly consumable by a small validator (`crates/contract/src/schema.rs`)
  with no spec parser.

The one thing OpenAPI would have given us — a route table — is a first-class
field of `server-api.json` instead.

## The artifacts

Regenerate with `scripts/export-bb-contract.sh <bb-checkout>`. Never edit them
by hand. `manifest.json` records the bb revision and the hash of every file.

| File | Kind | Contents |
| --- | --- | --- |
| `server-api.json` | `bb-http` | 167 routes with `request { source, schema }` and `responses [{ status, format, schema }]`, plus `errorResponse` (`apiErrorSchema`) and `lifecycleErrors` |
| `client-ws.json` | `bb-client-ws` | `client` and `terminal` protocols, subscription targets, change kinds |
| `host-daemon.json` | `bb-host-daemon` | daemon commands, results by type, WebSocket messages, enrollment/session/event/tool/interaction shapes, protocol version |
| `error-codes.json` | `bb-error-codes` | error code -> status inventory scanned from `apps/server/src` throw sites |
| `thread-event.json` | `bb-thread-event` | the complete `ThreadEvent` union and a schema for every `type` discriminator |

Shared definitions live in each file's `$defs`; every `$ref` is a local
`#/$defs/<name>` pointer, so a file is self-contained.

## Conformance testing

`crates/contract` embeds the artifacts and exposes lookups and validators:

```rust
let contract = loom_contract::Contract::load();
let route = contract.http_route("GET", "/api/v1/system/version").unwrap();
let violations = contract.validate_response(route, 200, &body);
assert!(violations.is_empty(), "{violations:?}");
```

`Violation` carries a JSON path, so a failure names the field that is wrong.
The same API covers client messages (`validate_client_message`), server
messages (`validate_server_message`) and daemon frames
(`validate_daemon_message`, `validate_server_to_daemon_message`).
Thread projection code can query `thread_event_schema("item/started")` or
validate a complete event with `validate_thread_event`.

`crates/contract/tests/conformance.rs` guards the artifacts themselves: refs
resolve, every JSON route has a response schema, and the validator accepts
contract-shaped values and rejects malformed ones.

## Request conformance is enforced at runtime

A response can be asserted in a test because the test can see it. A request
cannot: a handler that silently reshapes its body still returns the right JSON,
so response assertions stay green while a client sending the contract's shape
is rejected. That is exactly what happened to B1's write routes (W-554), which
accepted `{ "project_id": ... }` while `threads.create` requires
`{ "projectId", "origin", "input", "environment" }`.

Two mechanisms close it:

1. `loom-server` wires `validate_contract_request` as middleware. For every
   contract route whose `request.source` is `json`, the parsed body is
   validated before the handler runs, and a mismatch is a `422` in the uniform
   `{ code, message }` error shape naming the offending field. Contract-external
   loom routes are untouched. Live matching needs `Contract::match_route`, which
   treats the contract's `:id` parameters as wildcards.
2. Every implemented JSON-body route has a `validate_request_by_id` assertion in
   `crates/contract/tests/conformance.rs`, and
   `scripts/check-api-coverage.mjs` fails if such a route lacks one. That is the
   regression guard: the coverage number cannot claim a request conformance the
   tests do not prove.

`Contract::shared()` returns the process-wide parse so the middleware does not
re-parse the artifacts per request.

### Adding a route and keeping both sides consistent

1. Run `scripts/export-bb-contract.sh <bb-checkout>` in the same change that
   pulls a new bb revision. The artifacts and manifest update together. The
   checkout's HEAD must be the revision the manifest records; the manifest
   stamps `git rev-parse HEAD`, so exporting from a different revision silently
   changes the pin.
2. Implement the handler in `loom-server`, taking the contract's request shape
   directly (camelCase, required fields required). Do not accept a loom-only
   dialect beside it.
3. Add a conformance test that captures the handler's actual response and
   asserts it against the contract route, and a `validate_request_by_id`
   assertion for the request shape it accepts and one it must refuse (see the
   module comment in `crates/contract/tests/conformance.rs`).
4. If the route cannot conform, that is a **contract change, not a test
   waiver** — decide deliberately, document it here, and if it affects the UI
   surface reconsider the divergence.

The export fails loudly when a bb export is renamed (`schemaFrom` throws) and
reports type-only responses under `failures.opaqueResponseTypes`, so the Rust
side can never silently conform to a contract that has drifted.

## Known limits

- **Error codes are best-effort.** bb's contract package types the error body
  (`{ code, message, details?, retryable? }`) but leaves `code` a free string;
  `error-codes.json` is scanned from throw sites under `apps/server`.
- **`pattern` is carried but not enforced** by the Rust validator: it has no
  regex dependency, and a wrong dialect is worse than no check.
- **Recursive types lose precision.** A zod schema that recurses cannot be
  inlined; the recursion point becomes an unconstrained schema. This is rare
  (a handful of plugin/json-value shapes) and noted in the manifest.
- **The export is large-ish (~1.2 MB) and generated.** Repeated substructures
  are interned into `$defs` to keep it reviewable; a diff that touches a domain
  shape will still touch several places.
- **The `properties` key is never interned to a `$ref`.** The intern pass must
  not treat a property map as a schema; before W-554 it did, turning 87 shapes
  (the `threads.create`/`projects.create` request bodies among them) into
  `"properties": { "$ref": ... }`. The Rust validator reads that as an empty
  property set with a stray key, so every valid instance was reported as a
  violation. `tools/contract-export/src/intern.ts` handles `properties`
  specially and the conformance test `every_declared_schema_resolves` would not
  have caught it.
