/**
 * The loom HTTP routes the product app is allowed to call.
 *
 * This table is the app-side half of the same-origin boundary: every entry
 * mirrors one route in the exported bb contract (`contracts/bb/server-api.json`)
 * and the ids are the contract's own `id` field, so a route cannot be renamed
 * on one side only. `src/loom/api-client.test.ts` re-reads the contract and
 * fails if this table drifts.
 *
 * It is a hand-maintained subset on purpose. loom exports zod schemas and
 * request/response types instead of bb's Hono client (`createApiClient` needs
 * `@bb/hono-typed-routes` and the Hono runtime, which this fork deliberately
 * does not ship), so the transport needs an explicit route table to build URLs
 * and to refuse a route the contract does not declare.
 *
 * The table is the single source of truth for the transport: the method a
 * request uses, the path it addresses, and the parameter names it requires are
 * all derived from here rather than from the call site.
 */

export type LoomApiMethod = "GET" | "POST" | "PUT" | "PATCH" | "DELETE";

export interface LoomApiRoute {
  /** The contract route id (`system.config`, `threads.storageContent`, …). */
  readonly id: string;
  readonly method: LoomApiMethod;
  /** Contract-relative path, e.g. `/threads/:id/thread-storage/content`. */
  readonly path: string;
}

/** Every contract route lives under this prefix. */
export const LOOM_API_MOUNT_PATH = "/api/v1";

export const LOOM_API_ROUTES = [
  { id: "filePreviews.content", method: "GET", path: "/file-previews/:id/:filePath{.+}" },
  { id: "environments.diffFile", method: "GET", path: "/environments/:id/diff/file" },
  { id: "environments.get", method: "GET", path: "/environments/:id" },
  { id: "hosts.createJoinCode", method: "POST", path: "/hosts/join-codes" },
  { id: "hosts.list", method: "GET", path: "/hosts" },
  { id: "hosts.updatePermissionCeiling", method: "PATCH", path: "/hosts/:id/permission-ceiling" },
  { id: "projects.attachmentContent", method: "GET", path: "/projects/:id/attachments/content" },
  { id: "projects.branchOptions", method: "GET", path: "/projects/:id/branch-options" },
  { id: "projects.defaultExecutionOptions", method: "GET", path: "/projects/:id/default-execution-options" },
  { id: "projects.fileContent", method: "GET", path: "/projects/:id/files/content" },
  { id: "projects.sidebarBootstrap", method: "GET", path: "/sidebar-bootstrap" },
  { id: "system.environmentProviders", method: "GET", path: "/system/environment-providers" },
  { id: "system.executionOptions", method: "GET", path: "/system/execution-options" },
  { id: "system.providers", method: "GET", path: "/system/providers" },
  { id: "system.providerStates", method: "GET", path: "/system/providers/state" },
  { id: "system.version", method: "GET", path: "/system/version" },
  { id: "system.config", method: "GET", path: "/system/config" },
  { id: "system.uiPreferences", method: "GET", path: "/preferences/ui" },
  { id: "system.updateUiPreference", method: "PUT", path: "/preferences/ui/:key" },
  { id: "system.resetUiPreference", method: "DELETE", path: "/preferences/ui/:key" },
  { id: "system.voiceTranscription", method: "POST", path: "/system/voice-transcription" },
  { id: "threads.create", method: "POST", path: "/threads" },
  { id: "threads.defaultExecutionOptions", method: "GET", path: "/threads/:id/default-execution-options" },
  { id: "threads.get", method: "GET", path: "/threads/:id" },
  { id: "threads.hostFileContent", method: "GET", path: "/threads/:id/host-files/content" },
  { id: "threads.interactions", method: "GET", path: "/threads/:id/interactions" },
  { id: "threads.interaction", method: "GET", path: "/threads/:id/interactions/:interactionId" },
  { id: "threads.rawFile", method: "GET", path: "/threads/:id/files/raw" },
  { id: "threads.read", method: "POST", path: "/threads/:id/read" },
  { id: "threads.resolveInteraction", method: "POST", path: "/threads/:id/interactions/:interactionId/resolve" },
  { id: "threads.cancelInteraction", method: "POST", path: "/threads/:id/interactions/:interactionId/cancel" },
  { id: "threads.send", method: "POST", path: "/threads/:id/send" },
  { id: "threads.storageContent", method: "GET", path: "/threads/:id/thread-storage/content" },
  { id: "threads.storageFile", method: "GET", path: "/threads/:id/thread-storage/files/:filePath{.+}" },
  { id: "threads.storageFiles", method: "GET", path: "/threads/:id/thread-storage/files" },
  { id: "threads.storageLocation", method: "GET", path: "/threads/:id/thread-storage/location" },
  { id: "threads.storagePaths", method: "GET", path: "/threads/:id/thread-storage/paths" },
  { id: "threads.tabs", method: "GET", path: "/threads/:id/tabs" },
  { id: "threads.timeline", method: "GET", path: "/threads/:id/timeline" },
  { id: "threads.unread", method: "POST", path: "/threads/:id/unread" },
  { id: "threads.updateTabs", method: "PUT", path: "/threads/:id/tabs" },
  { id: "threads.worktreeFile", method: "GET", path: "/threads/:id/worktree/files/:filePath{.+}" },
] as const satisfies readonly LoomApiRoute[];

export type LoomApiRouteId = (typeof LOOM_API_ROUTES)[number]["id"];

/** The table row for a route id, so method/path types stay literal. */
export type LoomApiRouteOf<Id extends LoomApiRouteId> = Extract<
  (typeof LOOM_API_ROUTES)[number],
  { id: Id }
>;

export type LoomApiMethodOf<Id extends LoomApiRouteId> =
  LoomApiRouteOf<Id>["method"];

export type LoomApiPathOf<Id extends LoomApiRouteId> = LoomApiRouteOf<Id>["path"];

/**
 * Extract the parameter names a contract path declares.
 *
 * `"/threads/:id/worktree/files/:filePath{.+}"` → `"id" | "filePath"`, with the
 * `{.+}` catch-all suffix stripped. This is what makes the typed seam demand
 * exactly the parameters the route needs: a missing `id` is a compile error,
 * not a 404 at runtime.
 */
export type LoomApiPathParams<Path extends string> =
  Path extends `${string}:${infer Rest}`
    ? Rest extends `${infer Name}/${infer Tail}`
      ? LoomApiParamName<Name> | LoomApiPathParams<`/${Tail}`>
      : LoomApiParamName<Rest>
    : never;

type LoomApiParamName<Segment extends string> =
  Segment extends `${infer Name}{${string}}` ? Name : Segment;

/** Whether a path declares at least one `:param`. */
export type LoomApiHasParams<Id extends LoomApiRouteId> =
  LoomApiPathParams<LoomApiPathOf<Id>> extends never ? false : true;

const ROUTE_BY_ID = new Map<string, LoomApiRoute>(
  LOOM_API_ROUTES.map((route) => [route.id, route]),
);

export function findLoomApiRoute(id: string): LoomApiRoute | undefined {
  return ROUTE_BY_ID.get(id);
}

/** `/threads/:id/worktree/files/:filePath{.+}` → `["threads", ":id", "worktree", "files", ":filePath{.+}"]` */
export function routePathSegments(route: LoomApiRoute): string[] {
  return route.path.split("/").filter((segment) => segment.length > 0);
}

export function isPathParameter(segment: string): boolean {
  return segment.startsWith(":");
}

/** `:filePath{.+}` → `filePath`, so the caller's param bag uses the bare name. */
export function pathParameterName(segment: string): string {
  return segment.slice(1).replace(/\{.*\}$/u, "");
}

/** True when the segment is a `{.+}` catch-all (it may span `/`). */
export function isCatchAllParameter(segment: string): boolean {
  return segment.includes("{.+}");
}
