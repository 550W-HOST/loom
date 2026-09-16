import {
  findLoomApiRoute,
  isPathParameter,
  LOOM_API_MOUNT_PATH,
  pathParameterName,
  routePathSegments,
  type LoomApiRouteId,
} from "./loom-api-routes";
import { appSurfaceRequestInit } from "./app-surface";

/**
 * The same-origin typed HTTP client for the loom server.
 *
 * The app has no server-address setting: the origin is the server, and every
 * request is derived from it. `apiClient` in `./api-server` used to be an
 * all-throwing Proxy; this module is the real transport behind the subset of
 * contract routes the product app calls.
 */

export class LoomApiRouteError extends Error {
  readonly code = "loom_api_unknown_route";

  constructor(readonly routeId: string) {
    super(`Unknown loom contract route: ${routeId}`);
    this.name = "LoomApiRouteError";
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
  method?: string;
}

function resolveOrigin(): string {
  if (typeof window !== "undefined" && window.location?.origin) {
    return window.location.origin;
  }
  // Server-side rendering and unit tests have no origin. A relative URL is not
  // enough for `new URL`, so this placeholder is immediately replaced by the
  // route path; it is never sent anywhere.
  return "http://localhost";
}

/** The server origin every request derives from. */
export function loomApiOrigin(): string {
  return resolveOrigin();
}

function encodePathParam(value: string): string {
  return encodeURIComponent(value);
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

  const url = new URL(`${LOOM_API_MOUNT_PATH}${route.path}`, resolveOrigin());
  const segments: string[] = [];
  for (const segment of routePathSegments(route)) {
    if (!isPathParameter(segment)) {
      segments.push(segment);
      continue;
    }
    const name = pathParameterName(segment);
    const value = args.param?.[name];
    if (value === undefined) {
      throw new LoomApiRouteError(`${routeId} (missing :${name})`);
    }
    segments.push(
      segment.includes("{.+}")
        ? value.split("/").map(encodePathParam).join("/")
        : encodePathParam(value),
    );
  }
  url.pathname = `${LOOM_API_MOUNT_PATH}/${segments.join("/")}`;

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

function parseErrorBody(
  rawBody: string,
  contentType: string | null,
): unknown {
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

/**
 * Turn a non-2xx response into a `LoomHttpError`.
 *
 * The body is read, not discarded: a 404 and a 422 must stay distinguishable
 * and a JSON `{ code }` must survive. Aborts are re-thrown untouched so an
 * `AbortController` still cancels the query it belongs to.
 */
export async function throwLoomHttpError(response: Response): Promise<never> {
  const rawBody = await response.text().catch(() => "");
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

/** Perform a contract request and return the raw `Response`. */
export async function loomApiFetch(
  routeId: LoomApiRouteId,
  args: LoomRequestArgs = {},
): Promise<Response> {
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
      method: args.method ?? "GET",
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
