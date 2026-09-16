import type { LoomApiRouteId } from "./loom-api-routes";
import { buildLoomApiRelativeUrl, loomApiFetch } from "./loom-http";

/**
 * The `apiClient.<area>.<method>.$get(...)` seam the ported bb call sites use.
 *
 * bb's real client is Hono's `hc<PublicApiRoutes>` (`createApiClient`), which
 * needs the Hono runtime this fork does not ship. The product app instead calls
 * a small typed surface built from the contract route table: the method chain
 * resolves to a known contract route id, and the request goes out same-origin
 * through `./loom-http`.
 *
 * A chain that does not resolve to a route in the table throws
 * `LoomApiUnknownRouteError` instead of silently issuing a request — an
 * unimplemented or mistyped path must fail loudly, not return fabricated data.
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

export interface LoomApiCallArgs {
  param?: Record<string, string>;
  query?: Record<string, string | number | boolean | undefined>;
  json?: unknown;
  formData?: FormData;
  init?: { signal?: AbortSignal };
}

/**
 * Map the bb call-chain to a contract route id.
 *
 * The segments are the bb client's own path pieces (e.g.
 * `threads[":id"]["thread-storage"].content`), normalised into a dotted id that
 * matches the exported contract ids with the route table's own vocabulary.
 */
const CHAIN_TO_ROUTE_ID: Record<string, LoomApiRouteId> = {
  "sidebar-bootstrap": "projects.sidebarBootstrap",
  "system.config": "system.config",
  "system.voice-transcription": "system.voiceTranscription",
  "hosts.:id.permission-ceiling": "hosts.updatePermissionCeiling",
  "hosts.join-codes": "hosts.createJoinCode",
  "projects.:id.branch-options": "projects.branchOptions",
  "projects.:id.attachments.content": "projects.attachmentContent",
  "projects.:id.files.content": "projects.fileContent",
  "environments.:id.diff.file": "environments.diffFile",
  "threads.:id.thread-storage.content": "threads.storageContent",
  "threads.:id.thread-storage.files.:filePath": "threads.storageFile",
  "threads.:id.host-files.content": "threads.hostFileContent",
  "threads.:id.files.raw": "threads.rawFile",
  "threads.:id.worktree.files.:filePath": "threads.worktreeFile",
};

export { LoomHttpError } from "./loom-http";

type CallMethod = "$get" | "$post" | "$put" | "$patch" | "$delete" | "$url";

class LoomApiCall {
  constructor(
    private readonly chain: readonly string[],
    private readonly method: CallMethod,
  ) {}

  /** Bind the `$get`/`$post`/…/`$url` property the call chain asked for. */
  dispatch(): (...args: unknown[]) => unknown {
    switch (this.method) {
      case "$url":
        return (args) => this.$url((args ?? {}) as LoomApiCallArgs);
      case "$get":
        return (args, options) =>
          this.$get(args as LoomApiCallArgs | undefined, options);
      case "$post":
        return (args, options) =>
          this.$post(args as LoomApiCallArgs | undefined, options);
      case "$put":
        return (args, options) =>
          this.$put(args as LoomApiCallArgs | undefined, options);
      case "$patch":
        return (args, options) =>
          this.$patch(args as LoomApiCallArgs | undefined, options);
      case "$delete":
        return (args, options) =>
          this.$delete(args as LoomApiCallArgs | undefined, options);
    }
  }

  private routeId(): LoomApiRouteId {
    const key = this.chain.join(".");
    const routeId = CHAIN_TO_ROUTE_ID[key];
    if (routeId === undefined) {
      throw new LoomApiUnknownRouteError(`${key}.${this.method}`);
    }
    return routeId;
  }

  $url(args: LoomApiCallArgs = {}): URL {
    return new URL(
      buildLoomApiRelativeUrl(this.routeId(), {
        param: args.param,
        query: args.query,
      }),
      typeof window === "undefined" ? "http://localhost" : window.location.origin,
    );
  }

  $get(args?: LoomApiCallArgs, _options?: unknown): Promise<Response> {
    return this.send("GET", args, _options);
  }

  $post(args?: LoomApiCallArgs, _options?: unknown): Promise<Response> {
    return this.send("POST", args, _options);
  }

  $put(args?: LoomApiCallArgs, _options?: unknown): Promise<Response> {
    return this.send("PUT", args, _options);
  }

  $patch(args?: LoomApiCallArgs, _options?: unknown): Promise<Response> {
    return this.send("PATCH", args, _options);
  }

  $delete(args?: LoomApiCallArgs, _options?: unknown): Promise<Response> {
    return this.send("DELETE", args, _options);
  }

  private send(
    method: string,
    args: LoomApiCallArgs | undefined,
    options: unknown,
  ): Promise<Response> {
    const routeId = this.routeId();
    const signal =
      args?.init?.signal ??
      (options as { init?: { signal?: AbortSignal } } | undefined)?.init
        ?.signal;
    return loomApiFetch(routeId, {
      method,
      param: args?.param,
      query: args?.query,
      json: args?.json,
      formData: args?.formData,
      signal,
    });
  }
}

/**
 * The bb client writes a path parameter as an index expression
 * (`threads[":id"]`), so a property may arrive wrapped in quotes and brackets.
 * Normalising it keeps the route table readable.
 */
function normalizeChainSegment(property: string): string {
  return property.replace(/^\["?|"?\]$/gu, "").replace(/\{.*\}$/u, "");
}

function buildChain(segments: readonly string[]): unknown {
  return new Proxy(
    {},
    {
      get(_target, property) {
        if (typeof property !== "string") {
          return undefined;
        }
        if (
          property === "$get" ||
          property === "$post" ||
          property === "$put" ||
          property === "$patch" ||
          property === "$delete" ||
          property === "$url"
        ) {
          return new LoomApiCall(segments, property as CallMethod).dispatch();
        }
        return buildChain([...segments, normalizeChainSegment(property)]);
      },
    },
  );
}

export const apiClient: any = buildChain([]);

export function toRelativeUrl(url: URL): string {
  return `${url.pathname}${url.search}${url.hash}`;
}
