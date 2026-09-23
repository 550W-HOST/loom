import type {
  CommandListResponse,
  ProjectCommandsQuery,
  ProjectPathsQuery,
  WorkspacePathListResponse,
} from "@bb/server-contract";
import { loomApiJson } from "@/lib/loom-http";

export interface LoomProjectCommandsArgs extends ProjectCommandsQuery {
  projectId: string;
  signal?: AbortSignal;
}

/** Lists the slash commands available in a project workspace. */
export function loomProjectCommands(
  request: LoomProjectCommandsArgs,
): Promise<CommandListResponse> {
  const { projectId, signal, ...query } = request;
  return loomApiJson("projects.commands", {
    param: { id: projectId },
    query,
    signal,
  });
}

/**
 * The project workspace read the prompt's `@` file menu issues over loom.
 *
 * `projects.paths` was still the fail-closed browser SDK stub, so the mention
 * menu's path suggestions rejected with `BrowserSdkUnavailableError` and the
 * popover showed "Failed to load suggestions" against a server that answers
 * `200`. The route is a contract `GET`: the routing selector (`environmentId`
 * or `hostId`), the query, the limit and the two path-kind flags all travel as
 * query parameters, so the reader passes them through unchanged.
 *
 * It follows the loom-native reader pattern (`loom-system-readers.ts`,
 * `loom-thread-storage.ts`): a contract `GET`, wired into `src/lib/sdk.ts`.
 */

export interface LoomProjectPathsArgs extends ProjectPathsQuery {
  projectId: string;
  signal?: AbortSignal;
}

export function loomProjectPaths(
  request: LoomProjectPathsArgs,
): Promise<WorkspacePathListResponse> {
  const { projectId, signal, ...query } = request;
  return loomApiJson("projects.paths", {
    param: { id: projectId },
    query,
    signal,
  });
}
