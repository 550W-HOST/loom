import type {
  EnvironmentPathsQuery,
  WorkspacePathListResponse,
} from "@bb/server-contract";
import { loomApiJson } from "@/lib/loom-http";

/**
 * The environment workspace read the prompt's `@` file menu issues over loom.
 *
 * A thread with an environment routes its path suggestions through
 * `environments.paths` instead of `projects.paths`. That method was still the
 * fail-closed browser SDK stub, so the same `BrowserSdkUnavailableError`
 * reached the mention menu's error state. The contract route is a `GET` to
 * `/environments/:id/paths`; the query, limit and two path-kind flags travel as
 * query parameters.
 *
 * It follows the loom-native reader pattern (`loom-system-readers.ts`,
 * `loom-thread-storage.ts`): a contract `GET`, wired into `src/lib/sdk.ts`.
 */

export interface LoomEnvironmentPathsArgs extends EnvironmentPathsQuery {
  environmentId: string;
  signal?: AbortSignal;
}

export function loomEnvironmentPaths(
  request: LoomEnvironmentPathsArgs,
): Promise<WorkspacePathListResponse> {
  const { environmentId, signal, ...query } = request;
  return loomApiJson("environments.paths", {
    param: { id: environmentId },
    query,
    signal,
  });
}
