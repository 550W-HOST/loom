import type {
  CreateThreadSectionRequest,
  DeleteThreadSectionRequest,
  ThreadSectionMutationResponse,
  ThreadSectionResponse,
  UpdateThreadSectionRequest,
} from "@bb/server-contract";
import { loomApiJson } from "@/lib/loom-http";

/**
 * The thread-section mutations the product app issues over loom.
 *
 * `threadSections.create`, `.update` and `.delete` were still the fail-closed
 * browser SDK stubs, so creating, renaming or removing a sidebar section threw
 * `BrowserSdkUnavailableError` instead of reaching the server. All three
 * contract routes are mounted on `/thread-sections` with the id in the body
 * rather than the path, and their server handlers already exist, so this is the
 * app half only.
 *
 * It follows the loom-native writer pattern (`loom-project-mutations.ts`,
 * `loom-ui-preferences.ts`): the route table decides the verb and the body, and
 * each is wired into `src/lib/sdk.ts`.
 */

export interface LoomCreateThreadSectionArgs
  extends CreateThreadSectionRequest {
  signal?: AbortSignal;
}

/** Create a section; the contract answers `201` with the new section. */
export function loomCreateThreadSection(
  args: LoomCreateThreadSectionArgs,
): Promise<ThreadSectionResponse> {
  const { signal, ...json } = args;
  return loomApiJson("threadSections.create", { json, signal });
}

export interface LoomUpdateThreadSectionArgs
  extends UpdateThreadSectionRequest {
  signal?: AbortSignal;
}

/** Rename a section; the contract answers with the section and its thread count. */
export function loomUpdateThreadSection(
  args: LoomUpdateThreadSectionArgs,
): Promise<ThreadSectionMutationResponse> {
  const { signal, ...json } = args;
  return loomApiJson("threadSections.update", { json, signal });
}

export interface LoomDeleteThreadSectionArgs
  extends DeleteThreadSectionRequest {
  signal?: AbortSignal;
}

/** Remove a section; the contract carries the id in the DELETE body. */
export function loomDeleteThreadSection(
  args: LoomDeleteThreadSectionArgs,
): Promise<ThreadSectionMutationResponse> {
  const { signal, ...json } = args;
  return loomApiJson("threadSections.delete", { json, signal });
}
