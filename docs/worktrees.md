# Managed Git worktrees

A **managed worktree** is an environment whose workspace loom provisions as an
isolated `git worktree` cut from a project source on the environment's host.
It is the loom equivalent of bb's bundled `environment-git-worktree` plugin,
reimplemented as a first-class capability: loom has no plugin runtime, so the
provider is a server-side selection plus a worker-side git operation, not a
plugin.

This document is the design and the phase plan. It is the source of truth for
what is implemented and what is not.

## Implementation status

| Piece | State |
| --- | --- |
| Environment model fields (`provider_id`, `branch_name`, `base_branch`, `default_branch`, `is_git_repo`, `teardown`) | ☑ |
| Durable provisioned-workspace record (`environment_updated` event) | ☑ |
| `EnvironmentProvision` worktree inputs and report details | ☑ |
| Worker `git worktree add` + `.worktreeinclude` + completion marker | ☑ |
| HTTP `POST /api/v1/environments` provider/base-branch fields | ☑ |
| `git-worktree` in `GET /api/v1/system/environment-providers` | ☑ |
| Environment projection (`isWorktree`, `branchName`, `baseBranch`, `environmentProviderId`) | ☑ |
| `environment_value` / thread-list fields distinguish personal from worktree | ☑ |
| Teardown (`git worktree remove`) on `DELETE /api/v1/environments/{id}` | ☑ |
| App UI: create a thread on a `git-worktree` provider | ☑ |
| Named base branch passed through (inputs `{branch:{kind:"named",name}}`) | ☑ |
| `.bb-env-setup.sh` / `.bb-env-teardown.sh` lifecycle scripts | ☐ (not planned) |
| Remote base-branch fetch and ref-mutation retries | ☐ (not planned) |
| Attempt-scoped path keys and orphan recovery | ☐ (path is `<workspace-root>/<environment_id>`) |
| Archive grace period / retire-at / undo | ☐ (not planned) |

## Decisions

### The provider is a selection, not a plugin

`Environment.provider_id` names the capability that owns the workspace:

- `git-worktree` — a managed git worktree cut from a project source.
- `personal-workspace` — a managed empty directory under the worker's
  workspace root (the pre-worktree behaviour).
- `project-checkout` — an unmanaged absolute path the operator already has.

A create request that omits `provider_id` keeps the old defaults (`managed` →
`personal-workspace`, `unmanaged` → `project-checkout`), so existing clients
and test call sites are unchanged.

### The worker owns the layout and the git operation

The control plane sends a `GitWorktree` workspace in `EnvironmentProvision`
(source path, branch name, base branch) and the worker performs the git work.
The server never touches the host filesystem; the path is decided by the worker
under its `--workspace-root` and learned from the report.

The worker guarantees replay safety explicitly: `<environment_id>` is its
ownership boundary. A target that is already a worktree on the expected branch
is success; a target on the expected branch without the completion marker
re-runs `.worktreeinclude`; anything else fails rather than deleting unknown
data. `git worktree add` is not idempotent the way `create_dir_all` was, so the
old doc claim is no longer true for worktrees.

### Provisioning details are durable through a whole-entity event

`EnvironmentStatusChanged` carries only the status. Path and branch are the
ownership record teardown needs, so provisioning completion also emits
`EnvironmentUpdated { environment }`, which `apply_event` upserts whole. A
restart that recovers by replay therefore restores the path and branch, not
just `ready`.

### Teardown is a second request, not a side effect of `destroyed`

`DELETE /api/v1/environments/{id}` moves the record to `destroyed` and
publishes an `EnvironmentDeprovision` to the host. The worker runs
`git worktree remove --force` (plus `rm -rf` for the managed directory) and
reports; the outcome is recorded as `teardown` on the environment so a
failure is visible instead of silent. Loom removes only what it owns:
`personal-workspace` directories under the worker root and `git-worktree`
worktrees; an unmanaged path is never touched.

### Protocol version

Extending `EnvironmentProvision` and `EnvironmentProvisionOutcome` changes the
worker contract, so `PROTOCOL_VERSION` is bumped to 5. An old worker must not
silently provision a worktree as an empty directory.

## Paths and branch names

- Worktree path: `<workspace_root>/<environment_id>` (flat, not attempt-scoped).
- Marker: `<workspace_root>/<environment_id>.loom-completed` — written after
  `.worktreeinclude` finishes; its first line is the branch name.
- Branch: the caller's `branch_name`, or `loom/<environment_id>` when omitted.
  The server is the only party that mints it.
- Base branch: the caller's `base_branch`, or the source's default branch as
  the worker resolves it (preferring `origin/HEAD`, then the source's current
  branch). The worker records what it used as `base_branch` and which default
  it observed as `default_branch`.

## `.worktreeinclude`

Same semantics as bb: if the source checkout has a `.worktreeinclude` file with
at least one non-comment pattern, the worker runs

```
git ls-files --others --ignored --exclude-from=<source>/.worktreeinclude -z
```

in the source checkout and copies each listed file into the worktree,
preserving relative paths. Symlinks and destinations that already exist are
skipped; a destination whose parent escapes the worktree is skipped; per-file
errors are collected, not fatal. Tracked files are not copied — they arrive
from git.

## Further work

Each unchecked row above is a deliberate omission, not a missing seam:

- Lifecycle scripts need a process-group runner and streamed output, which the
  worker's `scripts` module already has the shape for. Today the agent starts in
  the worktree directly and no setup script runs.
- Remote fetches need a longer timeout than `run_git`'s 30s and the ref-mutation
  lock bb uses to make concurrent `fetch`/`worktree add` safe. Today a base
  branch that exists only on the remote is refused with a reason.
- Attempt-scoped path keys are only worth adding with retry-on-crash recovery,
  which the flat path plus the completion marker handles adequately today.
- Retire-at/undo needs a scheduler; `lifecycle.retireAt` in the API projection
  stays `null` until one exists.
- Automation runs still refuse `WorkspaceKind::ManagedWorktree`: provisioning
  is asynchronous and the automation executor does not wait for a worker yet
  (`crates/server/src/automation_execution.rs`). The environment entity now
  carries the branch and base, so this is waiting logic, not new data.
