/**
 * The loom-native routes the product shell reads.
 *
 * These are deliberately *not* in `contracts/bb/server-api.json`: loom owns
 * liveness and build identity, and `docs/api-coverage.md` lists `/health` and
 * `/api/v1/version` under "契约外 loom 路由". They are kept in a separate table
 * from `./loom-api-routes` so the contract-bound routes stay verifiable against
 * the exported contract, while these are verified against the Rust `.route()`
 * declarations in `crates/server/src/http.rs`.
 */

export interface LoomNativeRoute {
  /** Local identifier; never a bb contract id. */
  readonly id: string;
  readonly method: "GET";
  /** Absolute path including the mount prefix where applicable. */
  readonly path: string;
}

export const LOOM_NATIVE_ROUTES = [
  { id: "loom.health", method: "GET", path: "/health" },
  { id: "loom.version", method: "GET", path: "/api/v1/version" },
] as const satisfies readonly LoomNativeRoute[];

export type LoomNativeRouteId = (typeof LOOM_NATIVE_ROUTES)[number]["id"];

const NATIVE_ROUTE_BY_ID = new Map<string, LoomNativeRoute>(
  LOOM_NATIVE_ROUTES.map((route) => [route.id, route]),
);

export function findLoomNativeRoute(id: string): LoomNativeRoute | undefined {
  return NATIVE_ROUTE_BY_ID.get(id);
}
