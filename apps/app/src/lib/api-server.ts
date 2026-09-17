import {
  buildLoomApiRelativeUrl,
  loomApiFetch,
  resolveLoomApiMethod,
} from "./loom-http";
import type { LoomApiMethod, LoomApiMethodOf, LoomApiRouteId } from "./loom-api-routes";
import {
  type LoomApiArgsOptional,
  type LoomApiRequestArgs,
  type LoomApiUrlArgs,
  type LoomApiUrlArgsOptional,
} from "./loom-api-args";
import { LOOM_API_REQUEST_SPECS } from "./loom-api-request-spec";

/**
 * The `apiClient.<area>.<method>.$get(...)` seam the ported bb call sites use.
 *
 * bb's real client is Hono's `hc<PublicApiRoutes>` (`createApiClient`), which
 * needs the Hono runtime this fork does not ship. This is the loom-native
 * replacement, restricted to the routes in `./loom-api-routes`.
 *
 * The request shape comes from `./loom-api-args`, the same module the lowest
 * transport uses, so the two cannot disagree about what a route accepts.
 *
 * Two distinct argument shapes, because a URL and a request are not the same
 * thing:
 *
 * - `$url(...)` takes only the path parameters and query — never a body.
 * - The request methods (`$get`/`$post`/…) take those plus the body their
 *   contract declares, **required** for a `json`/`form` route. `$post()` with no
 *   body on a JSON route is a compile error, not a silently empty request.
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
export {
  LoomApiBodyNotAllowedError,
  LoomApiBodyRequiredError,
} from "./loom-http";

/** The options bag the ported call sites pass as the second argument. */
export interface LoomApiCallOptions {
  init?: { signal?: AbortSignal };
}

/** `$url` takes only what a URL can carry, and never a body. */
type LoomApiUrlTuple<Id extends LoomApiRouteId> =
  LoomApiUrlArgsOptional<Id> extends true
    ? [args?: LoomApiUrlArgs<Id>]
    : [args: LoomApiUrlArgs<Id>];

/**
 * A request method takes the URL arguments plus the body, and may only omit
 * them entirely when the route needs neither.
 */
type LoomApiRequestTuple<Id extends LoomApiRouteId> =
  LoomApiArgsOptional<Id> extends true
    ? [args?: LoomApiRequestArgs<Id>, options?: LoomApiCallOptions]
    : [args: LoomApiRequestArgs<Id>, options?: LoomApiCallOptions];

/**
 * Only the method the contract declares is present, so `$get` does not exist on
 * a `POST` route and `$post` does not exist on a `GET` route.
 */
type LoomApiRequestMethod<Id extends LoomApiRouteId> =
  LoomApiMethodOf<Id> extends "GET"
    ? { $get(...args: LoomApiRequestTuple<Id>): Promise<Response> }
    : LoomApiMethodOf<Id> extends "POST"
      ? { $post(...args: LoomApiRequestTuple<Id>): Promise<Response> }
      : LoomApiMethodOf<Id> extends "PUT"
        ? { $put(...args: LoomApiRequestTuple<Id>): Promise<Response> }
        : LoomApiMethodOf<Id> extends "PATCH"
          ? { $patch(...args: LoomApiRequestTuple<Id>): Promise<Response> }
          : { $delete(...args: LoomApiRequestTuple<Id>): Promise<Response> };

export type LoomApiCall<Id extends LoomApiRouteId> = {
  $url(...args: LoomApiUrlTuple<Id>): URL;
} & LoomApiRequestMethod<Id>;

/** The argument bag at runtime, before the transport gates it. */
interface LoomApiRuntimeArgs {
  param?: Readonly<Record<string, string>>;
  query?: Record<string, unknown>;
  json?: unknown;
  formData?: FormData;
  signal?: AbortSignal;
}

/** Exposed for tests: the request source the transport will enforce. */
export function requestSourceOf(routeId: LoomApiRouteId) {
  return LOOM_API_REQUEST_SPECS[routeId].source;
}

function createLoomApiCall<Id extends LoomApiRouteId>(
  routeId: Id,
): LoomApiCall<Id> {
  function send(
    verb: LoomApiMethod,
    args: LoomApiRuntimeArgs | undefined,
  ): Promise<Response> {
    // The type surface already exposed only this route's declared verb, so a
    // mismatch means untyped code (or a cast) reached here. Refuse it as a
    // rejected promise rather than a synchronous throw: this function is
    // declared to return one, and a sync throw would escape a caller's
    // `.catch()`.
    try {
      resolveLoomApiMethod(routeId, verb);
    } catch (error) {
      return Promise.reject(error);
    }
    // The transport independently enforces the body rules, so passing a body
    // here changes nothing about whether it is allowed.
    return loomApiFetch(
      routeId,
      {
        ...args,
        param: args?.param as never,
        query: args?.query as never,
        json: args?.json,
        formData: args?.formData,
      } as never,
    );
  }

  function toUrl(args: LoomApiRuntimeArgs | undefined): URL {
    const relative = buildLoomApiRelativeUrl(routeId, {
      param: args?.param as Record<string, string> | undefined,
      query: args?.query as
        | Record<string, string | number | boolean | undefined>
        | undefined,
    });
    return new URL(relative, loomApiOriginValue());
  }

  return {
    $url: (args?: LoomApiRuntimeArgs) => toUrl(args),
    $get: (args?: LoomApiRuntimeArgs) => send("GET", args),
    $post: (args?: LoomApiRuntimeArgs) => send("POST", args),
    $put: (args?: LoomApiRuntimeArgs) => send("PUT", args),
    $patch: (args?: LoomApiRuntimeArgs) => send("PATCH", args),
    $delete: (args?: LoomApiRuntimeArgs) => send("DELETE", args),
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
 * Every entry here is also in `LOOM_API_ROUTES`, and every entry has a request
 * spec; `src/loom/api-client.test.ts` asserts all three tables agree.
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
    list: route("hosts.list"),
  },
  projects: {
    ":id": {
      "branch-options": route("projects.branchOptions"),
      attachments: { content: route("projects.attachmentContent") },
      files: { content: route("projects.fileContent") },
    },
  },
  system: {
    config: route("system.config"),
    "voice-transcription": route("system.voiceTranscription"),
  },
  threads: {
    ":id": {
      "host-files": { content: route("threads.hostFileContent") },
      interactions: {
        ...route("threads.interactions"),
        ":interactionId": {
          ...route("threads.interaction"),
          cancel: route("threads.cancelInteraction"),
          resolve: route("threads.resolveInteraction"),
        },
      },
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
