/**
 * The loom HTTP routes the product app is allowed to call.
 *
 * This table is the app-side half of the same-origin boundary: every entry
 * mirrors one route in the exported bb contract (`contracts/bb/server-api.json`)
 * and the ids are the contract's own `id` field, so a route cannot be renamed
 * on one side only. `src/loom/api-route-table.test.ts` re-reads the contract and
 * fails if this table drifts.
 *
 * It is a hand-maintained subset on purpose. loom exports zod schemas and
 * request/response types instead of bb's Hono client (`createApiClient` needs
 * `@bb/hono-typed-routes` and the Hono runtime, which this fork deliberately
 * does not ship), so the transport needs an explicit route table to build URLs
 * and to refuse a route the contract does not declare.
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
  { id: "hosts.createJoinCode", method: "POST", path: "/hosts/join-codes" },
  { id: "hosts.updatePermissionCeiling", method: "PATCH", path: "/hosts/:id/permission-ceiling" },
  { id: "projects.attachmentContent", method: "GET", path: "/projects/:id/attachments/content" },
  { id: "projects.branchOptions", method: "GET", path: "/projects/:id/branch-options" },
  { id: "projects.fileContent", method: "GET", path: "/projects/:id/files/content" },
  { id: "projects.sidebarBootstrap", method: "GET", path: "/sidebar-bootstrap" },
  { id: "system.config", method: "GET", path: "/system/config" },
  { id: "system.voiceTranscription", method: "POST", path: "/system/voice-transcription" },
  { id: "threads.hostFileContent", method: "GET", path: "/threads/:id/host-files/content" },
  { id: "threads.rawFile", method: "GET", path: "/threads/:id/files/raw" },
  { id: "threads.storageContent", method: "GET", path: "/threads/:id/thread-storage/content" },
  { id: "threads.storageFile", method: "GET", path: "/threads/:id/thread-storage/files/:filePath{.+}" },
  { id: "threads.worktreeFile", method: "GET", path: "/threads/:id/worktree/files/:filePath{.+}" },
] as const satisfies readonly LoomApiRoute[];

export type LoomApiRouteId = (typeof LOOM_API_ROUTES)[number]["id"];

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
