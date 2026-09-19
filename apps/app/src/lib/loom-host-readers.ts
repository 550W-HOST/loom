import type {
  HostDirectoryListing,
  HostDirectoryQuery,
} from "@bb/server-contract";
import { loomApiJson } from "@/lib/loom-http";

/**
 * The host reads the product app issues over loom.
 *
 * `hosts.directory` was still the fail-closed browser SDK stub, so the remote
 * path browser (the New Machine / project path pickers) threw
 * `BrowserSdkUnavailableError` instead of listing a folder. The contract route
 * is a `GET` to `/hosts/:id/directory` whose `path` query is optional — the
 * server falls back to the host's reported default directory — so an absent
 * path omits the query rather than sending `"undefined"`.
 *
 * It follows the loom-native reader pattern (`loom-system-readers.ts`,
 * `loom-thread-storage.ts`): a contract `GET`, wired into `src/lib/sdk.ts`. The
 * server route already exists and is contract-tested, so this is the app half
 * only.
 */

export interface LoomHostDirectoryArgs extends HostDirectoryQuery {
  hostId: string;
  signal?: AbortSignal;
}

export function loomHostDirectory(
  request: LoomHostDirectoryArgs,
): Promise<HostDirectoryListing> {
  const { signal, hostId, ...query } = request;
  return loomApiJson("hosts.directory", {
    param: { id: hostId },
    query,
    signal,
  });
}
