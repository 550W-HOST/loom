import type { ProjectBranchesResponse } from "@bb/server-contract";
import type { ProjectBranchesArgs } from "@bb/sdk/browser";
import { request, requestOptions } from "./api";
import { apiClient } from "./api-server";

/**
 * Read a project's branch options.
 *
 * `@bb/sdk/browser` still declares this method's result as the compile-only
 * `any` (its `CompileOnlyResult` alias), so the app states the contract-derived
 * response type itself. The transport is the same-origin `apiClient` route
 * table, which refuses any path the exported contract does not declare.
 */
export function readProjectBranchOptions(
  input: ProjectBranchesArgs,
): Promise<ProjectBranchesResponse> {
  const { projectId, signal, ...query } = input;
  return request<ProjectBranchesResponse>(
    apiClient.projects[":id"]["branch-options"].$get(
      {
        param: { id: projectId },
        query,
      },
      requestOptions(signal),
    ),
  );
}
