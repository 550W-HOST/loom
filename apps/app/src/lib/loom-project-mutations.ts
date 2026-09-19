import type { CreateProjectRequest, ProjectResponse } from "@bb/server-contract";
import { loomApiJson } from "@/lib/loom-http";

/**
 * The project mutations the product app issues over loom.
 *
 * `projects.create` and `projects.delete` were still the fail-closed browser
 * SDK stubs, so creating a project from the New Project dialog and removing one
 * — from Settings or from a project row's actions — threw
 * `BrowserSdkUnavailableError` instead of reaching the server. The contract
 * routes (`projects.create`, a JSON `POST` to `/projects`, and `projects.delete`,
 * a bodyless `DELETE` to `/projects/:id`) and their server handlers already
 * exist, so this is the app half only.
 *
 * They follow the loom-native writer pattern (`loom-host-mutations.ts`,
 * `loom-ui-preferences.ts`): the route table decides the verb and the body, and
 * both are wired into `src/lib/sdk.ts`.
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
