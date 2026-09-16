import {
  buildLoomApiRelativeUrl,
  loomApiFetch,
  resolveLoomApiMethod,
  type LoomApiRouteArgs,
} from "./loom-http";
import type {
  LoomApiHasParams,
  LoomApiMethod,
  LoomApiMethodOf,
  LoomApiRouteId,
} from "./loom-api-routes";

/**
 * The `apiClient.<area>.<method>.$get(...)` seam the ported bb call sites use.
 *
 * bb's real client is Hono's `hc<PublicApiRoutes>` (`createApiClient`), which
 * needs the Hono runtime this fork does not ship. This is the loom-native
 * replacement, restricted to the routes in `./loom-api-routes`, and it is
 * *typed* rather than `any`:
 *
 * - each chain is bound to one contract route id, once;
 * - the `$url`/`$get`/`$post`/… arguments are derived from that route's path, so
 *   a missing `:id` or a stray parameter is a compile error;
 * - a route exposes only the method its contract declares, so a body cannot be
 *   attached to a `GET` (which the browser rejects outright) and a `POST` route
 *   cannot be read with `$get`.
 *
 * A chain that is not in the table is a type error, not a runtime surprise.
 */

export class LoomApiUnavailableError extends Error {
  readonly code = "loom_api_unavailable";

  constructor(readonly operation: string) {
    super(`Loom product API is not connected yet: ${operation}`);
    this.name = "LoomApiUnavailableError";
  }
}

export class LoomApiUnknownRouteError extends Error {
  readonly code = "loom_api_unknown_route";

  constructor(readonly operation: string) {
    super(`Loom product API has no contract route for: ${operation}`);
    this.name = "LoomApiUnknownRouteError";
  }
}

export { LoomHttpError, LoomApiMethodError, LoomApiPathParamError } from "./loom-http";

/** The options bag the ported call sites pass as the second argument. */
export interface LoomApiCallOptions {
  init?: { signal?: AbortSignal };
}

interface LoomApiCallMethods<Id extends LoomApiRouteId> {
  $url(...args: LoomApiArgsTuple<Id>): URL;
}

/**
 * A route with no `:param` takes an optional arguments bag; a route with one
 * requires it, so the parameter cannot be forgotten.
 */
type LoomApiArgsTuple<Id extends LoomApiRouteId> = LoomApiHasParams<Id> extends true
  ? [args: LoomApiRouteArgs<Id>, options?: LoomApiCallOptions]
  : [args?: LoomApiRouteArgs<Id>, options?: LoomApiCallOptions];

/**
 * Only the method the contract declares is present, so `$get` does not exist on
 * a `POST` route and `$post` does not exist on a `GET` route.
 */
type LoomApiRequestMethod<Id extends LoomApiRouteId> =
  LoomApiMethodOf<Id> extends "GET"
    ? { $get(...args: LoomApiArgsTuple<Id>): Promise<Response> }
    : LoomApiMethodOf<Id> extends "POST"
      ? { $post(...args: LoomApiArgsTuple<Id>): Promise<Response> }
      : LoomApiMethodOf<Id> extends "PUT"
        ? { $put(...args: LoomApiArgsTuple<Id>): Promise<Response> }
        : LoomApiMethodOf<Id> extends "PATCH"
          ? { $patch(...args: LoomApiArgsTuple<Id>): Promise<Response> }
          : { $delete(...args: LoomApiArgsTuple<Id>): Promise<Response> };

export type LoomApiCall<Id extends LoomApiRouteId> = LoomApiCallMethods<Id> &
  LoomApiRequestMethod<Id>;

interface LoomApiCallParams {
  param?: Record<string, string>;
  query?: Record<string, string | number | boolean | undefined>;
  json?: unknown;
  formData?: FormData;
}

function createLoomApiCall<Id extends LoomApiRouteId>(
  routeId: Id,
): LoomApiCall<Id> {
  function send(
    verb: LoomApiMethod,
    args: LoomApiCallParams | undefined,
    options: LoomApiCallOptions | undefined,
  ): Promise<Response> {
    // The declared method wins, and a mismatch is refused. The type surface
    // already hides the wrong verb, so reaching here with one means untyped
    // code (or a cast) tried to send a body with a GET.
    //
    // The refusal is wrapped in a rejected promise because this method is
    // declared to return one: throwing synchronously would escape a caller's
    // `.catch()` and surface as an unhandled error instead of a failed request.
    try {
      resolveLoomApiMethod(routeId, verb);
    } catch (error) {
      return Promise.reject(error);
    }
    return loomApiFetch(routeId, {
      param: args?.param,
      query: args?.query,
      json: args?.json,
      formData: args?.formData,
      signal: options?.init?.signal,
    });
  }

  function toUrl(args: LoomApiCallParams | undefined): URL {
    const relative = buildLoomApiRelativeUrl(routeId, {
      param: args?.param,
      query: args?.query,
    });
    return new URL(relative, loomApiOriginValue());
  }

  return {
    $url: (args?: LoomApiCallParams) => toUrl(args),
    $get: (args?: LoomApiCallParams, options?: LoomApiCallOptions) =>
      send("GET", args, options),
    $post: (args?: LoomApiCallParams, options?: LoomApiCallOptions) =>
      send("POST", args, options),
    $put: (args?: LoomApiCallParams, options?: LoomApiCallOptions) =>
      send("PUT", args, options),
    $patch: (args?: LoomApiCallParams, options?: LoomApiCallOptions) =>
      send("PATCH", args, options),
    $delete: (args?: LoomApiCallParams, options?: LoomApiCallOptions) =>
      send("DELETE", args, options),
  } as unknown as LoomApiCall<Id>;
}

function loomApiOriginValue(): string {
  return typeof window === "undefined" || !window.location?.origin
    ? "http://localhost"
    : window.location.origin;
}

function route<Id extends LoomApiRouteId>(id: Id): LoomApiCall<Id> {
  return createLoomApiCall(id);
}

/**
 * The routes the product app calls, each bound to its contract id.
 *
 * Every entry here is also in `LOOM_API_ROUTES`; `src/loom/api-client.test.ts`
 * asserts the two stay in step, so a route cannot be reachable without being
 * declared (or declared without being reachable).
 */
export const apiClient = {
  environments: {
    ":id": {
      diff: { file: route("environments.diffFile") },
    },
  },
  hosts: {
    ":id": {
      "permission-ceiling": route("hosts.updatePermissionCeiling"),
    },
    "join-codes": route("hosts.createJoinCode"),
  },
  projects: {
    ":id": {
      "branch-options": route("projects.branchOptions"),
      attachments: { content: route("projects.attachmentContent") },
      files: { content: route("projects.fileContent") },
    },
  },
  system: {
    "voice-transcription": route("system.voiceTranscription"),
  },
  threads: {
    ":id": {
      "host-files": { content: route("threads.hostFileContent") },
      "thread-storage": {
        content: route("threads.storageContent"),
        files: { ":filePath{.+}": route("threads.storageFile") },
      },
      files: { raw: route("threads.rawFile") },
      worktree: { files: { ":filePath{.+}": route("threads.worktreeFile") } },
    },
  },
} as const;

export function toRelativeUrl(url: URL): string {
  return `${url.pathname}${url.search}${url.hash}`;
}
