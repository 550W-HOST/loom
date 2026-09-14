# Projects and sources

A project is the top-level container. A thread belongs to exactly one project,
and so does an environment: the project is what a UI lists, and what owns the
workspaces its threads run in.

## The seeded personal project

The server seeds **one** project when it first starts, with
`kind: "personal"` and the name `Personal`. The decision, stated plainly:

> The personal project is seeded once and then treated as an ordinary project.
> It is not a hidden fallback.

Concretely:

- it is returned by `GET /api/v1/projects` like any other project;
- it can be renamed, given sources, and archived;
- **`kind` is provenance, not privilege.** Nothing in the command API behaves
  differently for it. A client renders it however it likes — the reference UI
  simply lists it with the rest;
- a thread and an environment must **name** their project. The thread route
  takes the contract's `projectId` (absent is a `422` from the request
  validator), the environment route its own `project_id` (absent is a `400`);
  neither lands silently in the personal project. That is what removes the "one
  implicit project" failure mode this change exists to fix.

The seeded project's creation is deliberately **not** published as a
`project_created` event. It is part of the registry's construction, and like
the seeded project's id itself it travels in the domain snapshot rather than in
the log. See [`domain-persistence.md`](domain-persistence.md). A client that
wants every project including the seeded one uses `GET /api/v1/projects`;
`global` only carries projects created while a client was listening (and the
replay window, for as long as that stretches).

## Sources

`Project.sources` is a list of `ProjectSource`, one entry per host. On the wire
it is bb's `projectSourceSchema`, camelCase and `type`-tagged:

```json
{
  "id": "src_…",
  "projectId": "proj_…",
  "hostId": "host_…",
  "type": "local_path",
  "path": "/srv/loom",
  "isDefault": true,
  "createdAt": 1789120438372,
  "updatedAt": 1789120438372
}
```

- **`hostId` is required.** A source says where the code lives *on a machine*.
  Two sources for the same project on two machines is the normal multi-machine
  shape; a source naming a host that was never enrolled is a `404`.
- **`path` may be empty** for a source that only declares a repository before
  any checkout exists. A source with neither a path nor a remote is rejected.
- **Recording a remote is a declaration, not an action.** Adding a source never
  clones or fetches. Materialising a workspace from a source is environment
  provisioning's job, and it consumes whatever the source declared.
- The **first source added becomes the default** (`isDefault: true`). Removing
  the default promotes the first remaining source, so a project with sources
  always has exactly one default.

## Lifecycle events

| change | event | scope |
| --- | --- | --- |
| create a project | `project_created` | `global` |
| rename, set remote, add/remove a source | `project_updated` | `project:{id}` |
| archive a project | `project_updated` | `project:{id}` |

One event, one scope, exactly as the [event model](event-model.md) requires.
`project_created` goes to `global` because the project list is not scoped to a
project a client can already have subscribed to; every later change lands in
the project's own room. A client therefore follows a project by subscribing to
`global` and then to `project:{id}` once it knows the id.

## Archiving

> A project with a run in flight cannot be archived. Idle threads are not
> cascaded; they keep their project reference.

- **Refused, not cascaded.** If any thread in the project is `working` or
  `waiting`, `POST /api/v1/projects/{id}/archive` is a `409`. Those threads have
  a provider running against a workspace inside the project, and silently
  archiving or moving them would leave a run writing into a project the UI no
  longer shows.
- **Idle, errored and archived threads do not block it.** They stay exactly
  where they are and keep referencing the archived project, so their history
  still resolves.
- **Archiving is terminal for the record.** It is not a delete: the project, its
  sources and its events remain, and threads/environments that reference it keep
  resolving. A second archive is a `409` (`DomainError::Archived`), and every
  mutation — rename, set remote, add/remove source — is rejected from then on.
- Creating a thread or an environment in an archived project is rejected
  (`409`), so an archived project cannot collect new work.

There is deliberately **no unarchive command** in this change. Archiving being
one-way keeps the state machine small; if a real need appears, an `unarchive`
transition is additive.

## Deleting

`DELETE /api/v1/projects/{id}` is a **tombstone**, not a removal, and it is
stronger than archiving:

- An archived project is still listed and still resolves by id; a deleted one is
  in neither `projects.list` nor the sidebar bootstrap, and a route that resolves
  it by id answers `404 project_not_found`.
- The record stays in the registry and in snapshots (`deleted_at_ms`), so
  replaying an older `project_created` cannot resurrect it — the same reason a
  thread has a `deleted_at_ms`.
- It refuses while the project still holds a **live thread or a live
  environment** (`409 conflict`). Archiving only refuses a *running* thread
  because those threads keep referencing the project and their history still
  resolves; a delete would leave them naming a project the client can no longer
  open.
- Archiving being the weaker state, an archived project can still be deleted —
  refusing that would strand it as undeletable.

## Ordering

`PATCH /api/v1/projects/{id}/order` moves a project between two neighbours, both
nullable: `previousProjectId: null` means "first" and `nextProjectId: null`
means "last". `Project.sort_key` is a sparse base-62 rank, and the list orders by
archived status, then the rank, then creation time and id.

A project that has never been reordered has no rank and still sorts by creation
time, so an existing workspace looks unchanged. The first reorder (and any
reorder whose bounding neighbours are unranked) rewrites the whole visible list
into contiguous ranks; after that every project holds one and later moves take
the cheap single-row path. A reorder with a stale neighbour, or between a pair
already in the requested order, is a `409 conflict` rather than a silent no-op.

## Prompt history, commands and attachments

- `projects.promptHistory` aggregates the user prompts of the project's threads
  from the same `thread_message_added` events `threads.promptHistory` reads, so
  the two cannot disagree about what a prompt is.
- `projects.commands` asks the project's source host (`host.list_commands`),
  because a prompt-command list is a property of the workspace on disk. The rows
  are discovered by `pi-acp` and projected into bb's contract shape.
- `projects.files`, `projects.paths` and `projects.fileContent` read the
  project's workspace on its host. `projects.uploadAttachment` writes into
  `<host data_dir>/project-attachments/<project_id>`, and
  `projects.copyAttachments` copies between two projects' directories. A host
  that never reported a data directory answers `501 not_configured` rather than
  guessing a path. See [`contract.md`](contract.md) ("B7").

## Thread sections

A `ThreadSection` (`sec_…`) is a durable sidebar group, persisted in the domain
snapshot and listed in `sidebarBootstrap.sections`. Names are unique after
trimming (`409 section_name_conflict`); an unknown id is `404 section_not_found`.

The relationship is **one-sided**: `Thread.section_id` holds the section's id as
an opaque string, and nothing enforces that the section exists. Deleting a
section counts the threads that referenced it (`updatedThreadCount`) and does
**not** rewrite them — the thread's grouping is the client's last write, and a
delete that re-filed every thread would be an unrequested mutation of a
different entity. A client that wants its threads back in the default group
sends `threads.update` itself.

## HTTP surface

The read/write routes are bb's, in the contract's shapes: camelCase bodies, bare
arrays for lists, the created row at `201`.

```text
GET    /api/v1/projects                 → [projectSchema, …]
POST   /api/v1/projects                 { name, source: { type, hostId, path } } → 201 projectSchema
GET    /api/v1/projects/{id}            → projectSchema
PATCH  /api/v1/projects/{id}            { name? } → projectSchema
DELETE /api/v1/projects/{id}            → { ok } (tombstone)
POST   /api/v1/projects/{id}/sources    { type, hostId, path } → 201 projectSourceSchema
PATCH  /api/v1/projects/{id}/sources/{sourceId}  { type, path?, isDefault? } → projectSourceSchema
DELETE /api/v1/projects/{id}/sources/{sourceId}
PATCH  /api/v1/projects/{id}/order      { previousProjectId, nextProjectId } → [projectSchema, …]
GET    /api/v1/projects/{id}/files      → { files, truncated }
GET    /api/v1/projects/{id}/paths      → { paths, truncated }
GET    /api/v1/projects/{id}/files/content?path=…  → bytes
GET    /api/v1/projects/{id}/commands   → { commands }
GET    /api/v1/projects/{id}/prompt-history → [promptHistoryEntry, …]
POST   /api/v1/projects/{id}/attachments (form) → 201 uploadedAttachment
GET    /api/v1/projects/{id}/attachments/content?path=… → bytes
POST   /api/v1/projects/{id}/attachments/copy → { ok }
POST   /api/v1/projects/{id}/archive    (loom-native) → Project
POST   /api/v1/thread-sections          { name } → 201 threadSection
PATCH  /api/v1/thread-sections          { id, name } → { id, name, updatedThreadCount }
DELETE /api/v1/thread-sections          { id } → { id, name, updatedThreadCount }
```

The list is sorted by archived status, then by the client's explicit rank when
it set one, then by creation time and id. It never depends on `HashMap`
iteration order, and a project that has never been reordered looks exactly as it
did before `projects.reorder` existed. Deleted projects are omitted entirely.
`hostId` is required on both create calls: a project or source names the machine
its code lives on, and a host that was never enrolled is a `404` rather than an
unhosted source.

## Relationship to bb's contract

`projects.list`, `projects.create`, `projects.get`, `projects.update`,
`projects.createSource`, `projects.deleteSource`, the workspace file routes,
`projects.updateSource`, `projects.reorder`, `projects.delete`,
`projects.promptHistory`, `projects.commands`, the attachment routes and
`threadSections.*` are implemented in bb's shapes, so a bb client's project
calls reach them unchanged. Archive is the one route above returning the domain
`Project` instead of bb's `{ ok }`: bb has no archive verb, so there is no
contract shape to match. The remaining unimplemented project routes are skills
(decided out of scope) and nothing else; per-route status is tracked in
[`api-coverage.md`](api-coverage.md).

Archive is one of the loom-native control endpoints that
[`contract.md`](contract.md) says must move off the `/api/v1/*` prefix bb's
routes are reserved to once the client/daemon protocol split lands; until then
it is knowingly divergent and uncovered by the contract tests.
