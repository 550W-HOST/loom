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
- a thread and an environment must **name** their project. `project_id` is
  required on both create calls, and omitting it is a `400`, not a silent
  landing in the personal project. That is what removes the "one implicit
  project" failure mode this change exists to fix.

The seeded project's creation is deliberately **not** published as a
`project_created` event. It is part of the registry's construction, and like
the seeded project's id itself it travels in the domain snapshot rather than in
the log. See [`domain-persistence.md`](domain-persistence.md). A client that
wants every project including the seeded one uses `GET /api/v1/projects`;
`global` only carries projects created while a client was listening (and the
replay window, for as long as that stretches).

## Sources

`Project.sources` is a list of `ProjectSource`, one entry per host:

```json
{
  "id": "src_…",
  "project_id": "proj_…",
  "host_id": "host_…",
  "path": "/srv/loom",
  "git_remote_url": "git@github.com:550W-HOST/loom.git",
  "is_default": true,
  "created_at_ms": 1789120438372,
  "updated_at_ms": 1789120438372
}
```

- **`host_id` is required.** A source says where the code lives *on a
  machine*. Two sources for the same project on two machines is the normal
  multi-machine shape.
- **`path` may be empty** for a source that only declares a repository before
  any checkout exists. A source with neither a path nor a `git_remote_url` is
  rejected.
- **`git_remote_url` is a declaration, not an action.** Adding a source never
  clones or fetches. Materialising a workspace from a source is environment
  provisioning's job, and it consumes whatever the source declared.
- The **first source added becomes the default** (`is_default: true`). Removing
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

```text
GET    /api/v1/projects                              → { "projects": [Project, …] }
POST   /api/v1/projects                              { name, git_remote_url? }
GET    /api/v1/projects/{id}                         → Project
PATCH  /api/v1/projects/{id}                         { name?, git_remote_url? }
POST   /api/v1/projects/{id}/archive
POST   /api/v1/projects/{id}/sources                 { host_id?, path?, git_remote_url? }
DELETE /api/v1/projects/{id}/sources/{source_id}
```

The list is sorted by creation time and then id (active projects first,
archived last), so it never depends on `HashMap` iteration order. `host_id` on
a source defaults to the primary host, exactly like
`POST /api/v1/environments`; with no host enrolled the request is a `409`
rather than an unhosted source.

## Relationship to bb's contract

This surface is **loom-native**, not a subset of bb's `/api/v1/projects`
routes. bb models a project as a name plus a single `source` (a discriminated
`local_path`), and exposes it under `/projects` with different shapes and
lifecycle verbs (branches, skills, files, attachments). loom reuses its own
`Project` / `ProjectSource` domain types — one source per host, plural, with an
optional git remote — because the domain layer predates this issue and the
issue asks for those types to be reused rather than replaced.

It sits under the `/api/v1/*` prefix that [`contract.md`](contract.md) already
marks as knowingly divergent from bb until the client/daemon protocol split
lands, and it is one of the loom-native control endpoints that split has to
move under a distinct prefix.
