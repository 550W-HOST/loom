import type {
  LoomApiHasParams,
  LoomApiPathOf,
  LoomApiPathParams,
  LoomApiRouteId,
} from "./loom-api-routes";
import { LOOM_API_REQUEST_SPECS } from "./loom-api-request-spec";

/**
 * The arguments each route accepts, derived from its contract path and its
 * request source.
 *
 * This lives in its own module because **both** the lowest transport
 * (`loomApiFetch` / `loomApiJson`) and the call-chain seam (`apiClient`) must
 * apply the same rules. When only the seam was typed, the exported transport
 * still accepted a generic bag and attached a body to a route-table `GET`.
 *
 * The rules:
 *
 * - `param` is required exactly when the path declares a `:param`, and its keys
 *   are exactly the declared names.
 * - The body kind follows the contract's `request.source`: a `json` route takes
 *   a required JSON body, a `form` route a required `FormData`, a `query` route
 *   a typed query and **no** body, and a `none` route neither.
 * - `query` is never available on a route whose contract does not declare one.
 */

type LoomApiSpecOf<Id extends LoomApiRouteId> =
  (typeof LOOM_API_REQUEST_SPECS)[Id];

export type LoomApiQueryOf<Id extends LoomApiRouteId> =
  LoomApiSpecOf<Id> extends { source: "query"; query: infer TQuery }
    ? TQuery
    : never;

export type LoomApiJsonOf<Id extends LoomApiRouteId> =
  LoomApiSpecOf<Id> extends { source: "json"; json: infer TJson }
    ? TJson
    : never;

export type LoomApiFormOf<Id extends LoomApiRouteId> =
  LoomApiSpecOf<Id> extends { source: "form"; form: infer TForm }
    ? TForm
    : never;

/**
 * The parameter bag for a route: exactly its declared parameters, no others.
 *
 * A generic `Record<string, string>` would accept `{ wrongName: "a.ts" }` and
 * then fail at runtime with a missing `:id`; this makes the correct names the
 * only representable ones.
 */
export type LoomApiParamBagFor<Id extends LoomApiRouteId> = {
  [K in LoomApiPathParams<LoomApiPathOf<Id>>]: string;
};

/** The path parameters for a route, required iff the path declares any. */
type LoomApiParamPart<Id extends LoomApiRouteId> =
  LoomApiHasParams<Id> extends true
    ? { param: LoomApiParamBagFor<Id> }
    : { param?: never };

/** Whether the contract permits omitting a query object entirely. */
type LoomApiQueryOptional<Id extends LoomApiRouteId> =
  LoomApiSpecOf<Id> extends { source: "query" }
    ? {} extends LoomApiQueryOf<Id>
      ? true
      : false
    : true;

/** The query part: typed, and present only for a `query` route. */
type LoomApiQueryPart<Id extends LoomApiRouteId> =
  LoomApiSpecOf<Id> extends { source: "query" }
    ? LoomApiQueryOptional<Id> extends true
      ? { query?: LoomApiQueryOf<Id> }
      : { query: LoomApiQueryOf<Id> }
    : { query?: never };

/**
 * What a URL needs: the path parameters and the query.
 *
 * A body is deliberately absent, so `$url()` can never be handed one.
 */
export type LoomApiUrlArgs<Id extends LoomApiRouteId> = LoomApiParamPart<Id> &
  LoomApiQueryPart<Id> & { signal?: AbortSignal };

/**
 * The body part: required for a `json`/`form` route, forbidden otherwise.
 *
 * `json` and `formData` are mutually exclusive by construction — no route
 * declares both, and neither appears on a route that has no body.
 */
type LoomApiBodyPart<Id extends LoomApiRouteId> =
  LoomApiSpecOf<Id> extends { source: "json" }
    ? { json: LoomApiJsonOf<Id>; formData?: never }
    : LoomApiSpecOf<Id> extends { source: "form" }
      ? { formData: LoomApiFormOf<Id>; json?: never }
      : { json?: never; formData?: never };

/** Everything a request needs: the URL arguments plus its declared body. */
export type LoomApiRequestArgs<Id extends LoomApiRouteId> =
  LoomApiUrlArgs<Id> & LoomApiBodyPart<Id>;

/**
 * Whether a route's request arguments may be omitted entirely.
 *
 * A `json`/`form` route always needs them (the body is required); a route with
 * a `:param` needs them too. Everything else — a plain `GET` with no parameters
 * and no body — may be called with no arguments.
 */
export type LoomApiArgsOptional<Id extends LoomApiRouteId> =
  LoomApiHasParams<Id> extends true
    ? false
    : LoomApiSpecOf<Id> extends { source: "json" | "form" }
      ? false
      : LoomApiQueryOptional<Id>;

/**
 * Whether a route's **URL** arguments may be omitted.
 *
 * A URL never carries a body, so it only depends on the path: a route with a
 * `:param` needs arguments and one without does not. Deriving this from
 * `LoomApiArgsOptional` would make `$url()` require an argument on a body route
 * like voice transcription, whose URL needs nothing.
 */
export type LoomApiUrlArgsOptional<Id extends LoomApiRouteId> =
  LoomApiHasParams<Id> extends true ? false : LoomApiQueryOptional<Id>;

/** The exported transport's conditional argument tuple. */
export type LoomApiRequestTuple<Id extends LoomApiRouteId> =
  LoomApiArgsOptional<Id> extends true
    ? [args?: LoomApiRequestArgs<Id>]
    : [args: LoomApiRequestArgs<Id>];

/** Whether a body must be present for this route. */
export function requestNeedsBody(routeId: LoomApiRouteId): boolean {
  const source = LOOM_API_REQUEST_SPECS[routeId].source;
  return source === "json" || source === "form";
}
