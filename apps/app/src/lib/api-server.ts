import {
  buildLoomApiRelativeUrl,
  loomApiFetch,
  resolveLoomApiMethod,
} from "./loom-http";
import type {
  LoomApiHasParams,
  LoomApiMethod,
  LoomApiMethodOf,
  LoomApiPathOf,
  LoomApiPathParams,
  LoomApiRouteId,
} from "./loom-api-routes";
import {
  LOOM_API_REQUEST_SPECS,
  type LoomApiParamBag,
  type LoomApiRequestSource,
} from "./loom-api-request-spec";

/**
 * The `apiClient.<area>.<method>.$get(...)` seam the ported bb call sites use.
 *
 * bb's real client is Hono's `hc<PublicApiRoutes>` (`createApiClient`), which
 * needs the Hono runtime this fork does not ship. This is the loom-native
 * replacement, restricted to the routes in `./loom-api-routes`, and it is typed
 * per route rather than `any`:
 *
 * - a chain is bound to one contract route id, once;
 * - the path parameters come from that route's path, so a missing `:id` is a
 *   compile error;
 * - only the method the contract declares is exposed; and
 * - the request body kind follows the contract's `request.source`, so a `query`
 *   route takes a typed query and *no* body, a `json` route takes a typed body
 *   and no query, and a `form` route takes multipart. Supplying the wrong one —
 *   or both — does not compile.
 *
 * Those last two are the difference between "the browser rejects GET with a
 * body" and "the call cannot be written".
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

/**
 * A body was supplied to a route whose contract does not declare one.
 *
 * This is the runtime backstop for the type-level rule: reaching it means an
 * untyped caller (or a cast) tried to send a body with a `GET`, which the
 * browser refuses before the request leaves.
 */
export class LoomApiBodyNotAllowedError extends Error {
  readonly code = "loom_api_body_not_allowed";

  constructor(
    readonly routeId: string,
    readonly method: LoomApiMethod,
    readonly bodyKind: string,
  ) {
    super(
      `Route ${routeId} is ${method} and declares no ${bodyKind} body in the contract`,
    );
    this.name = "LoomApiBodyNotAllowedError";
  }
}

export { LoomHttpError, LoomApiMethodError, LoomApiPathParamError } from "./loom-http";

/** The options bag the ported call sites pass as the second argument. */
export interface LoomApiCallOptions {
  init?: { signal?: AbortSignal };
}

type LoomApiSpecOf<Id extends LoomApiRouteId> =
  (typeof LOOM_API_REQUEST_SPECS)[Id];

type LoomApiQueryOf<Id extends LoomApiRouteId> =
  LoomApiSpecOf<Id> extends { source: "query"; query: infer TQuery }
    ? TQuery
    : never;

type LoomApiJsonOf<Id extends LoomApiRouteId> =
  LoomApiSpecOf<Id> extends { source: "json"; json: infer TJson }
    ? TJson
    : never;

type LoomApiFormOf<Id extends LoomApiRouteId> =
  LoomApiSpecOf<Id> extends { source: "form"; form: infer TForm }
    ? TForm
    : never;

/**
 * The request arguments a route accepts, chosen by its contract body kind.
 *
 * A `query` route has no `json`/`formData` property at all; a `json` route has
 * no `query`; a route with `source: "none"` has neither. `param` is required
 * exactly when the path declares a `:param`.
 */
type LoomApiBodyArgs<Id extends LoomApiRouteId> =
  LoomApiSpecOf<Id> extends { source: "json" }
    ? { json: LoomApiJsonOf<Id> }
    : LoomApiSpecOf<Id> extends { source: "form" }
      ? { formData: LoomApiFormOf<Id> }
      : LoomApiSpecOf<Id> extends { source: "query" }
        ? { query?: LoomApiQueryOf<Id> }
        : { query?: never; json?: never; formData?: never };

type LoomApiArgsFor<Id extends LoomApiRouteId> = LoomApiBodyArgs<Id> & {
  param?: LoomApiHasParams<Id> extends true
    ? LoomApiParamBagFor<Id>
    : never;
};

/**
 * The parameter bag for a route: exactly its declared parameters, no others.
 *
 * A generic `Record<string, string>` would accept `{ wrongName: "a.ts" }` and
 * then fail at runtime with a missing `:id`; this makes the correct names the
 * only representable ones.
 */
type LoomApiParamBagFor<Id extends LoomApiRouteId> = {
  [K in LoomApiPathParams<LoomApiPathOf<Id>>]: string;
};

/** A route with a `:param` requires the argument bag; one without may omit it. */
export type LoomApiArgs<Id extends LoomApiRouteId> = LoomApiHasParams<Id> extends true
  ? LoomApiArgsFor<Id> & { param: LoomApiParamBagFor<Id> }
  : LoomApiArgsFor<Id>;

type LoomApiArgsTuple<Id extends LoomApiRouteId> = LoomApiHasParams<Id> extends true
  ? [args: LoomApiArgs<Id>, options?: LoomApiCallOptions]
  : [args?: LoomApiArgs<Id>, options?: LoomApiCallOptions];

interface LoomApiCallMethods<Id extends LoomApiRouteId> {
  $url(...args: LoomApiArgsTuple<Id>): URL;
}

/**
 * Only the method the contract declares is present, so `$get` does not exist on
 * a `POST` route and `$post` does not exist on a `GET` route.
 */
type LoomApiRequestMethod<Id extends LoomApiRouteId> =
  LoomApiMethodOf<Id> extends "GET"
    ? {
        $get(...args: LoomApiArgsTuple<Id>): Promise<Response>;
      }
    : LoomApiMethodOf<Id> extends "POST"
      ? {
          $post(...args: LoomApiArgsTuple<Id>): Promise<Response>;
        }
      : LoomApiMethodOf<Id> extends "PUT"
        ? {
            $put(...args: LoomApiArgsTuple<Id>): Promise<Response>;
          }
        : LoomApiMethodOf<Id> extends "PATCH"
          ? {
              $patch(...args: LoomApiArgsTuple<Id>): Promise<Response>;
            }
          : {
              $delete(...args: LoomApiArgsTuple<Id>): Promise<Response>;
            };

export type LoomApiCall<Id extends LoomApiRouteId> = LoomApiCallMethods<Id> &
  LoomApiRequestMethod<Id>;

/** The whole argument bag at runtime, before the spec gates it. */
interface LoomApiRuntimeArgs {
  param?: LoomApiParamBag;
  query?: Record<string, unknown>;
  json?: unknown;
  formData?: FormData;
}

export function requestSourceOf(routeId: LoomApiRouteId): LoomApiRequestSource {
  return LOOM_API_REQUEST_SPECS[routeId].source;
}

/**
 * Reject a body the contract does not declare for this route.
 *
 * Method is checked first: a `GET` with a body is the browser-level failure this
 * guards, and reporting it as a method/body conflict is clearer than reporting
 * it as a missing-query problem.
 */
function assertBodyAllowed(
  routeId: LoomApiRouteId,
  method: LoomApiMethod,
  args: LoomApiRuntimeArgs | undefined,
): void {
  const hasJson = args?.json !== undefined;
  const hasForm = args?.formData !== undefined;
  if (!hasJson && !hasForm) return;

  const source = requestSourceOf(routeId);
  if (hasJson && source !== "json") {
    throw new LoomApiBodyNotAllowedError(routeId, method, "json");
  }
  if (hasForm && source !== "form") {
    throw new LoomApiBodyNotAllowedError(routeId, method, "form");
  }
}

function createLoomApiCall<Id extends LoomApiRouteId>(
  routeId: Id,
): LoomApiCall<Id> {
  function send(
    verb: LoomApiMethod,
    args: LoomApiRuntimeArgs | undefined,
    options: LoomApiCallOptions | undefined,
  ): Promise<Response> {
    // The declared method wins and a mismatch is refused. The type surface
    // already hides the wrong verb; this catches untyped callers.
    //
    // Both refusals are wrapped in a rejected promise because this method is
    // declared to return one: throwing synchronously would escape a caller's
    // `.catch()` and surface as an unhandled error instead of a failed request.
    try {
      resolveLoomApiMethod(routeId, verb);
      assertBodyAllowed(routeId, verb, args);
    } catch (error) {
      return Promise.reject(error);
    }
    return loomApiFetch(routeId, {
      param: args?.param,
      query: args?.query as
        | Record<string, string | number | boolean | undefined>
        | undefined,
      json: args?.json,
      formData: args?.formData,
      signal: options?.init?.signal,
    });
  }

  function toUrl(args: LoomApiRuntimeArgs | undefined): URL {
    const relative = buildLoomApiRelativeUrl(routeId, {
      param: args?.param,
      query: args?.query as
        | Record<string, string | number | boolean | undefined>
        | undefined,
    });
    return new URL(relative, loomApiOriginValue());
  }

  return {
    $url: (args?: LoomApiRuntimeArgs) => toUrl(args),
    $get: (args?: LoomApiRuntimeArgs, options?: LoomApiCallOptions) =>
      send("GET", args, options),
    $post: (args?: LoomApiRuntimeArgs, options?: LoomApiCallOptions) =>
      send("POST", args, options),
    $put: (args?: LoomApiRuntimeArgs, options?: LoomApiCallOptions) =>
      send("PUT", args, options),
    $patch: (args?: LoomApiRuntimeArgs, options?: LoomApiCallOptions) =>
      send("PATCH", args, options),
    $delete: (args?: LoomApiRuntimeArgs, options?: LoomApiCallOptions) =>
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
