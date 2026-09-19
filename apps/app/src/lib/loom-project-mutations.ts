import { loomApiJson } from "@/lib/loom-http";

/**
 * The project mutations the product app issues over loom.
 *
 * `projects.delete` was still the fail-closed browser SDK stub, so removing a
 * project — from Settings or from a project row's actions — threw
 * `BrowserSdkUnavailableError` instead of reaching the server. The contract
 * route (`projects.delete`, a bodyless `DELETE` to `/projects/:id`) and its
 * server handler already exist, so this is the app half only.
 *
 * It follows the loom-native writer pattern (`loom-host-mutations.ts`,
 * `loom-ui-preferences.ts`): a contract DELETE with no body, wired into
 * `src/lib/sdk.ts`.
 */

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
