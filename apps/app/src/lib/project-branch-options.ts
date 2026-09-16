import type {
  ProjectBranchesQuery,
  ProjectBranchesResponse,
} from "@bb/server-contract";
import { request } from "./api";
import { apiClient } from "./api-server";

/**
 * Read a project's branch options.
 *
 * The argument is the contract's own `ProjectBranchesQuery` (query/limit) plus
 * the project id and signal, rather than `@bb/sdk/browser`'s
 * `[key: string]: unknown` bag: with the typed `apiClient` seam, an unknown
 * query key is a compile error instead of a parameter that silently never
 * reaches the server.
 */
export interface ProjectBranchOptionsArgs extends ProjectBranchesQuery {
  projectId: string;
  signal?: AbortSignal;
}

export function readProjectBranchOptions(
  input: ProjectBranchOptionsArgs,
): Promise<ProjectBranchesResponse> {
  const { projectId, signal, ...query } = input;
  return request<ProjectBranchesResponse>(
    apiClient.projects[":id"]["branch-options"].$get(
      {
        param: { id: projectId },
        query,
      },
      signal === undefined ? undefined : { init: { signal } },
    ),
  );
}
