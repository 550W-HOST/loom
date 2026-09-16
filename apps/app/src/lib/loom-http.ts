import {
  findLoomApiRoute,
  isCatchAllParameter,
  isPathParameter,
  LOOM_API_MOUNT_PATH,
  pathParameterName,
  routePathSegments,
  type LoomApiMethod,
  type LoomApiPathParams,
  type LoomApiPathOf,
  type LoomApiRouteId,
  type LoomApiHasParams,
} from "./loom-api-routes";
import { appSurfaceRequestInit } from "./app-surface";

/**
 * The same-origin typed HTTP client for the loom server.
 *
 * The app has no server-address setting: the origin is the server, and every
 * request is derived from it. This module is the real transport behind the
 * subset of contract routes the product app calls.
 *
 * Two rules are load-bearing and are enforced here rather than trusted to the
 * call site:
 *
 * 1. The route table decides the method. A request cannot send a body with
 *    `GET`, and a caller that asks for a method the contract does not declare
 *    is refused instead of silently issuing the wrong verb.
 * 2. Path parameters are taken raw and encoded exactly once. Pre-encoded input
 *    would be encoded a second time (`%20` → `%2520`), and `.`/`..` segments
 *    would be normalised by the `URL` constructor into a path outside the
 *    declared route, so both are rejected before the URL is built.
 */

export class LoomApiRouteError extends Error {
  readonly code = "loom_api_unknown_route";

  constructor(readonly routeId: string) {
    super(`Unknown loom contract route: ${routeId}`);
    this.name = "LoomApiRouteError";
  }
}

/** A path parameter was missing, empty, or unsafe to place in a path. */
export class LoomApiPathParamError extends Error {
  readonly code = "loom_api_invalid_path_param";

  constructor(
    readonly routeId: string,
    readonly paramName: string,
    readonly reason: string,
  ) {
    super(`Invalid path parameter :${paramName} for ${routeId}: ${reason}`);
    this.name = "LoomApiPathParamError";
  }
}

/** The caller asked for a method the route's contract does not declare. */
export class LoomApiMethodError extends Error {
  readonly code = "loom_api_method_mismatch";

  constructor(
    readonly routeId: string,
    readonly declared: LoomApiMethod,
    readonly requested: LoomApiMethod,
  ) {
    super(
      `Route ${routeId} is ${declared} in the contract, but the call asked for ${requested}`,
    );
    this.name = "LoomApiMethodError";
  }
}

export class LoomHttpError extends Error {
  readonly status: number;
  readonly code?: string;
  readonly body?: unknown;

  constructor(args: {
    status: number;
    message: string;
    code?: string;
    body?: unknown;
  }) {
    super(`HTTP ${args.status}: ${args.message}`);
    this.name = "LoomHttpError";
    this.status = args.status;
    this.code = args.code;
    this.body = args.body;
  }
}

export interface LoomRequestArgs {
  /** Path parameters keyed by the bare route name (`id`, `filePath`). */
  param?: Record<string, string>;
  /** Query string values; `undefined` entries are omitted. */
  query?: Record<string, string | number | boolean | undefined>;
  /** JSON request body. Mutually exclusive with `formData`. */
  json?: unknown;
  /** Multipart body, used by the voice-transcription route. */
  formData?: FormData;
  signal?: AbortSignal;
}

function resolveOrigin(): string {
  if (typeof window !== "undefined" && window.location?.origin) {
    return window.location.origin;
  }
  // Server-side rendering and unit tests have no origin. A relative URL is not
  // enough for `new URL`, so this placeholder stands in for it.
  return "http://localhost";
}

/** The server origin every request derives from. */
export function loomApiOrigin(): string {
  return resolveOrigin();
}

const FORBIDDEN_SEGMENT_CHARS = /[\u0000-\u001f\u007f\\]/u;

/**
 * Validate one raw path segment.
 *
 * `.` and `..` are refused rather than encoded: the `URL` constructor
 * normalises them (including the `%2E%2E` spelling) into a path outside the
 * declared route, so encoding them would not make the request safe.
 */
function validatePathSegment(
  routeId: string,
  paramName: string,
  segment: string,
): void {
  if (segment.length === 0) {
    throw new LoomApiPathParamError(routeId, paramName, "empty segment");
  }
  if (segment === "." || segment === "..") {
    throw new LoomApiPathParamError(
      routeId,
      paramName,
      `"." and ".." are not addressable path segments`,
    );
  }
  if (FORBIDDEN_SEGMENT_CHARS.test(segment)) {
    throw new LoomApiPathParamError(
      routeId,
      paramName,
      "control characters and backslashes are not allowed",
    );
  }
}

/** Encode a raw path parameter exactly once. */
function encodePathParam(
  routeId: string,
  paramName: string,
  value: string,
  catchAll: boolean,
): string {
  if (value.length === 0) {
    throw new LoomApiPathParamError(routeId, paramName, "empty value");
  }
  if (!catchAll) {
    validatePathSegment(routeId, paramName, value);
    return encodeURIComponent(value);
  }
  return value
    .split("/")
    .map((segment) => {
      validatePathSegment(routeId, paramName, segment);
      return encodeURIComponent(segment);
    })
    .join("/");
}

/**
 * Build the absolute request URL for a contract route.
 *
 * A `:filePath{.+}` catch-all keeps its `/` separators (each segment is encoded
 * individually) so a nested path still addresses the same route, while a plain
 * `:id` is encoded as one segment.
 */
export function buildLoomApiUrl(
  routeId: LoomApiRouteId,
  args: LoomRequestArgs = {},
): URL {
  const route = findLoomApiRoute(routeId);
  if (!route) {
    throw new LoomApiRouteError(routeId);
  }

  const segments: string[] = [];
  for (const segment of routePathSegments(route)) {
    if (!isPathParameter(segment)) {
      segments.push(segment);
      continue;
    }
    const name = pathParameterName(segment);
    const value = args.param?.[name];
    if (value === undefined) {
      throw new LoomApiPathParamError(routeId, name, "missing value");
    }
    segments.push(
      encodePathParam(routeId, name, value, isCatchAllParameter(segment)),
    );
  }

  const pathname = `${LOOM_API_MOUNT_PATH}/${segments.join("/")}`;
  const url = new URL(pathname, resolveOrigin());
  // Defence in depth: if anything still normalised the path, refuse rather than
  // request a route the contract never declared.
  if (url.pathname !== pathname) {
    throw new LoomApiPathParamError(
      routeId,
      "path",
      "resolved path left the declared route",
    );
  }

  for (const [key, value] of Object.entries(args.query ?? {})) {
    if (value === undefined) continue;
    url.searchParams.set(key, String(value));
  }

  return url;
}

/** The relative URL (`pathname + search`) the browser actually requests. */
export function buildLoomApiRelativeUrl(
  routeId: LoomApiRouteId,
  args: LoomRequestArgs = {},
): string {
  const url = buildLoomApiUrl(routeId, args);
  return `${url.pathname}${url.search}${url.hash}`;
}

function normalizeErrorText(raw: string): string {
  return raw.replace(/\s+/g, " ").trim();
}

function errorMessageFromBody(
  status: number,
  statusText: string,
  rawBody: string,
  contentType: string | null,
): string {
  const normalized = normalizeErrorText(rawBody);
  if (normalized.length === 0) {
    return statusText || "Request failed";
  }
  const looksJson =
    (contentType?.includes("application/json") ?? false) ||
    normalized.startsWith("{") ||
    normalized.startsWith("[");
  if (looksJson) {
    try {
      const parsed = JSON.parse(normalized) as unknown;
      const record =
        parsed !== null && typeof parsed === "object"
          ? (parsed as Record<string, unknown>)
          : null;
      for (const key of ["message", "detail", "error"] as const) {
        const value = record?.[key];
        if (typeof value === "string" && value.trim().length > 0) {
          return value;
        }
      }
    } catch {}
  }
  if (/<!doctype html|<html[\s>]/iu.test(normalized)) {
    return statusText || "Request failed";
  }
  return normalized;
}

function parseErrorBody(rawBody: string, contentType: string | null): unknown {
  const normalized = normalizeErrorText(rawBody);
  if (normalized.length === 0) return undefined;
  const looksJson =
    (contentType?.includes("application/json") ?? false) ||
    normalized.startsWith("{") ||
    normalized.startsWith("[");
  if (!looksJson) return undefined;
  try {
    return JSON.parse(normalized) as unknown;
  } catch {
    return undefined;
  }
}

function errorCodeFromBody(body: unknown): string | undefined {
  if (body === null || typeof body !== "object" || Array.isArray(body)) {
    return undefined;
  }
  const code = (body as Record<string, unknown>).code;
  return typeof code === "string" && code.trim().length > 0 ? code : undefined;
}

function isAbortLike(error: unknown): boolean {
  return (
    error instanceof DOMException && error.name === "AbortError"
  ) || (error instanceof Error && error.name === "AbortError");
}

/**
 * Turn a non-2xx response into a `LoomHttpError`.
 *
 * The body is read, not discarded: a 404 and a 422 must stay distinguishable
 * and a JSON `{ code }` must survive.
 *
 * Reading that body is itself abortable, so an abort raised while draining it
 * is re-thrown as an abort. Swallowing it would turn a cancelled request into a
 * bogus HTTP failure — and a cancelled request must stay cancelled.
 */
export async function throwLoomHttpError(response: Response): Promise<never> {
  let rawBody = "";
  try {
    rawBody = await response.text();
  } catch (error) {
    if (isAbortLike(error)) throw error;
    rawBody = "";
  }
  const contentType = response.headers.get("content-type");
  const body = parseErrorBody(rawBody, contentType);
  throw new LoomHttpError({
    status: response.status,
    message: errorMessageFromBody(
      response.status,
      response.statusText,
      rawBody,
      contentType,
    ),
    code: errorCodeFromBody(body),
    body,
  });
}

/**
 * Resolve the method a request will use.
 *
 * The route table wins. `requested` exists only so a call site that names a
 * method explicitly is checked against the contract rather than silently
 * overriding it: a mismatch is a programming error, not a fallback.
 */
export function resolveLoomApiMethod(
  routeId: LoomApiRouteId,
  requested?: LoomApiMethod,
): LoomApiMethod {
  const route = findLoomApiRoute(routeId);
  if (!route) {
    throw new LoomApiRouteError(routeId);
  }
  if (requested !== undefined && requested !== route.method) {
    throw new LoomApiMethodError(routeId, route.method, requested);
  }
  return route.method;
}

/** Perform a contract request and return the raw `Response`. */
export async function loomApiFetch(
  routeId: LoomApiRouteId,
  args: LoomRequestArgs = {},
): Promise<Response> {
  const method = resolveLoomApiMethod(routeId);
  const url = buildLoomApiUrl(routeId, args);
  const headers = new Headers();
  let body: BodyInit | undefined;

  if (args.formData !== undefined) {
    body = args.formData;
  } else if (args.json !== undefined) {
    headers.set("content-type", "application/json");
    body = JSON.stringify(args.json);
  }

  const response = await fetch(
    url,
    appSurfaceRequestInit({
      method,
      headers,
      body,
      signal: args.signal,
    }),
  );

  if (!response.ok) {
    await throwLoomHttpError(response);
  }
  return response;
}

/**
 * Perform a request against a loom-native path (`/health`), which is outside
 * the bb contract and therefore has no route-table entry.
 *
 * It shares the transport's error mapping with the contract routes so a 404 or
 * a backend failure is reported the same way on either side.
 */
export async function loomNativeJson<TResponse>(
  path: string,
  args: { signal?: AbortSignal } = {},
): Promise<TResponse> {
  const response = await fetch(
    new URL(path, resolveOrigin()),
    appSurfaceRequestInit({ method: "GET", signal: args.signal }),
  );
  if (!response.ok) {
    await throwLoomHttpError(response);
  }
  const text = await response.text();
  if (text.length === 0) {
    return undefined as TResponse;
  }
  return JSON.parse(text) as TResponse;
}

/** Perform a contract request and parse its JSON body. */
export async function loomApiJson<TResponse>(
  routeId: LoomApiRouteId,
  args: LoomRequestArgs = {},
): Promise<TResponse> {
  const response = await loomApiFetch(routeId, args);
  const text = await response.text();
  if (text.length === 0) {
    return undefined as TResponse;
  }
  return JSON.parse(text) as TResponse;
}

/**
 * The parameters a route requires, derived from its contract path.
 *
 * A route with no `:param` takes no `param` bag at all, so a caller cannot pass
 * a parameter the path does not declare (which would silently do nothing).
 */
export type LoomApiRouteArgs<Id extends LoomApiRouteId> = Omit<
  LoomRequestArgs,
  "param"
> &
  (LoomApiHasParams<Id> extends true
    ? {
        param: {
          [K in LoomApiPathParams<LoomApiPathOf<Id>>]: string;
        };
      }
    : { param?: undefined });
