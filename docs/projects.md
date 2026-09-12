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

## HTTP surface

The read/write routes are bb's, in the contract's shapes: camelCase bodies, bare
arrays for lists, the created row at `201`.

```text
GET    /api/v1/projects                 → [projectSchema, …]
POST   /api/v1/projects                 { name, source: { type, hostId, path } } → 201 projectSchema
GET    /api/v1/projects/{id}            → projectSchema
PATCH  /api/v1/projects/{id}            { name? } → projectSchema
POST   /api/v1/projects/{id}/sources    { type, hostId, path } → 201 projectSourceSchema
DELETE /api/v1/projects/{id}/sources/{sourceId}
POST   /api/v1/projects/{id}/archive    (loom-native) → Project
```

The list is sorted by creation time and then id (active projects first,
archived last), so it never depends on `HashMap` iteration order. `hostId` is
required on both create calls: a project or source names the machine its code
lives on, and a host that was never enrolled is a `404` rather than an unhosted
source.

## Relationship to bb's contract

`projects.list`, `projects.create`, `projects.get`, `projects.update`,
`projects.createSource` and `projects.deleteSource` are implemented in bb's
shapes, so a bb client's project calls reach them unchanged. Two things stay
loom's own: the archive verb, which bb does not have (that is why it is the one
route above returning the domain `Project`), and the routes this repository has
not implemented yet — branches, files, attachments, skills — which are tracked
per route in [`api-coverage.md`](api-coverage.md).

Archive is one of the loom-native control endpoints that
[`contract.md`](contract.md) says must move off the `/api/v1/*` prefix bb's
routes are reserved to once the client/daemon protocol split lands; until then
it is knowingly divergent and uncovered by the contract tests.
