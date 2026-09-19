import type {
  CreateProjectRequest,
  CreateProjectSourceRequest,
  ProjectResponse,
  ReorderProjectRequest,
  UpdateProjectRequest,
  UpdateProjectSourceRequest,
} from "@bb/server-contract";
import type { ProjectSource } from "@bb/domain";
import { loomApiJson } from "@/lib/loom-http";

/**
 * The project mutations the product app issues over loom.
 *
 * `projects.create` and `projects.delete` were still the fail-closed browser
 * SDK stubs, so creating a project from the New Project dialog and removing one
 * — from Settings or from a project row's actions — threw
 * `BrowserSdkUnavailableError` instead of reaching the server. The rest of the
 * project write surface was stubbed the same way: rename, the sidebar drag
 * reorder and the add/update/remove of a project's sources. The contract routes
 * and their server handlers already exist, so this is the app half only.
 *
 * They follow the loom-native writer pattern (`loom-host-mutations.ts`,
 * `loom-ui-preferences.ts`): the route table decides the verb and the body, and
 * each is wired into `src/lib/sdk.ts`.
 */

export interface LoomCreateProjectArgs extends CreateProjectRequest {
  signal?: AbortSignal;
}

/**
 * Create a project through the contract route.
 *
 * The contract declares a JSON body whose `source` names the machine the code
 * lives on and the local path, and answers `201` with the created project.
 * Returning that body keeps the SDK's `ProjectResponse` result honest rather
 * than inventing one; the caller reads its `id` to select the new project.
 */
export function loomCreateProject(
  args: LoomCreateProjectArgs,
): Promise<ProjectResponse> {
  const { signal, ...json } = args;
  return loomApiJson("projects.create", { json, signal });
}

export interface LoomUpdateProjectArgs extends UpdateProjectRequest {
  projectId: string;
  signal?: AbortSignal;
}

/** Rename a project; the contract body is the `name` field alone. */
export function loomUpdateProject(
  args: LoomUpdateProjectArgs,
): Promise<ProjectResponse> {
  const { projectId, signal, ...json } = args;
  return loomApiJson("projects.update", {
    param: { id: projectId },
    json,
    signal,
  });
}

export interface LoomReorderProjectArgs extends ReorderProjectRequest {
  projectId: string;
  signal?: AbortSignal;
}

/**
 * Place a project between two neighbours through the contract route.
 *
 * The server answers with the reordered list, so the result is the server's
 * ordering rather than the caller re-deriving it.
 */
export function loomReorderProject(
  args: LoomReorderProjectArgs,
): Promise<ProjectResponse[]> {
  const { projectId, signal, ...json } = args;
  return loomApiJson("projects.reorder", {
    param: { id: projectId },
    json,
    signal,
  });
}

export type LoomAddProjectSourceArgs = CreateProjectSourceRequest & {
  projectId: string;
  signal?: AbortSignal;
};

/**
 * Build the body for the source union from the chosen branch.
 *
 * Spreading the request would keep `remoteUrl` on a `local_path` source, and
 * the contract validates both branches strictly, so the body is rebuilt instead.
 */
function projectSourceAddJson(
  request: CreateProjectSourceRequest,
): CreateProjectSourceRequest {
  if (request.type === "local_path") {
    return { hostId: request.hostId, path: request.path, type: request.type };
  }
  return {
    hostId: request.hostId,
    type: request.type,
    ...(request.remoteUrl === undefined ? {} : { remoteUrl: request.remoteUrl }),
    ...(request.targetPath === undefined
      ? {}
      : { targetPath: request.targetPath }),
  };
}

export function loomAddProjectSource(
  args: LoomAddProjectSourceArgs,
): Promise<ProjectSource> {
  const { projectId, signal, ...request } = args;
  return loomApiJson("projects.createSource", {
    param: { id: projectId },
    json: projectSourceAddJson(request),
    signal,
  });
}

export interface LoomUpdateProjectSourceArgs
  extends UpdateProjectSourceRequest {
  projectId: string;
  sourceId: string;
  signal?: AbortSignal;
}

export function loomUpdateProjectSource(
  args: LoomUpdateProjectSourceArgs,
): Promise<ProjectSource> {
  const { projectId, sourceId, signal, ...json } = args;
  return loomApiJson("projects.updateSource", {
    param: { id: projectId, sourceId },
    json,
    signal,
  });
}

export interface LoomDeleteProjectSourceArgs {
  projectId: string;
  sourceId: string;
  signal?: AbortSignal;
}

export function loomDeleteProjectSource(
  args: LoomDeleteProjectSourceArgs,
): Promise<{ ok: true }> {
  return loomApiJson("projects.deleteSource", {
    param: { id: args.projectId, sourceId: args.sourceId },
    signal: args.signal,
  });
}

export interface LoomDeleteProjectArgs {
  projectId: string;
  signal?: AbortSignal;
}

/**
 * Delete a project through the contract route.
 *
 * Deletion is a tombstone, not a cascade: the server answers `{ ok: true }` on
 * success and refuses a project that still holds a live thread or environment
 * with a typed `409` (a second delete is a `404`) rather than deleting the
 * project's contents. Returning the parsed body keeps the SDK's `{ ok: true }`
 * result honest rather than inventing it.
 */
export function loomDeleteProject(
  args: LoomDeleteProjectArgs,
): Promise<{ ok: true }> {
  return loomApiJson("projects.delete", {
    param: { id: args.projectId },
    signal: args.signal,
  });
}
