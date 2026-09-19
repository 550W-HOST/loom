import type {
  ThreadStorageFileListResponse,
  ThreadStorageFilesQuery,
  ThreadStorageLocationResponse,
  ThreadStoragePathListResponse,
  ThreadStoragePathsQuery,
} from "@bb/server-contract";
import { loomApiJson } from "@/lib/loom-http";

/**
 * Thread-storage reads over loom.
 *
 * `threads.storageFiles` and `threads.storagePaths` list a thread's storage
 * directory on the host that owns its environment; `threads.storageLocation`
 * answers which host that is and where the root lives, from the entity view
 * rather than by asking the host. All three are contract GETs, so the filters
 * travel as a query string and the transport refuses a body.
 *
 * These are the loom-native replacements for the browser SDK's fail-closed
 * stubs, wired in `src/lib/sdk.ts`. The panel cannot render without them: the
 * file list, the path suggestions and the "reveal on host" absolute path each
 * read one of the three routes.
 */

export function loomThreadStorageFiles(
  request: ThreadStorageFilesQuery & { signal?: AbortSignal; threadId: string },
): Promise<ThreadStorageFileListResponse> {
  const { signal, threadId, ...query } = request;
  return loomApiJson("threads.storageFiles", {
    param: { id: threadId },
    query,
    signal,
  });
}

export function loomThreadStorageLocation(request: {
  signal?: AbortSignal;
  threadId: string;
}): Promise<ThreadStorageLocationResponse> {
  return loomApiJson("threads.storageLocation", {
    param: { id: request.threadId },
    signal: request.signal,
  });
}

export function loomThreadStoragePaths(
  request: ThreadStoragePathsQuery & { signal?: AbortSignal; threadId: string },
): Promise<ThreadStoragePathListResponse> {
  const { signal, threadId, ...query } = request;
  return loomApiJson("threads.storagePaths", {
    param: { id: threadId },
    query,
    signal,
  });
}
