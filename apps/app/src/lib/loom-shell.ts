import { queryOptions, useQuery } from "@tanstack/react-query";
import type {
  SidebarBootstrapResponse,
  SystemConfigResponse,
} from "@bb/server-contract";
import { loomApiJson, loomNativeJson } from "@/lib/loom-http";
import { findLoomNativeRoute } from "@/lib/loom-native-routes";
import {
  readCachedSidebarBootstrap,
  writeCachedSidebarBootstrap,
} from "@/lib/sidebar-bootstrap-cache";
import {
  sidebarNavigationQueryKey,
  systemConfigQueryKey,
} from "@/hooks/queries/query-keys";

/**
 * The product shell's own data boundary: health, sidebar bootstrap and system
 * config, read same-origin over the typed HTTP client.
 *
 * The sidebar and system-config entries deliberately reuse the ported query
 * keys rather than inventing parallel ones. The shell and the app then share a
 * single cache entry per resource, so "the shell loaded" and "the app has
 * data" cannot disagree. Only `health` is new, because nothing in the ported
 * app reads liveness today.
 */

export const LOOM_SHELL_HEALTH_QUERY_KEY = ["loom", "shell", "health"] as const;

/** The server's liveness payload (`GET /health`). */
export interface LoomHealthResponse {
  readonly status: string;
  readonly protocol_version: number;
  readonly node_id: string;
  readonly uptime_ms: number;
  readonly readers: number;
  readonly retained_events: number;
  readonly backend_error?: string;
}

export class LoomNativeRouteError extends Error {
  readonly code = "loom_native_route_missing";

  constructor(readonly routeId: string) {
    super(`Unknown loom-native route: ${routeId}`);
    this.name = "LoomNativeRouteError";
  }
}

/**
 * `findLoomNativeRoute` guards the id, so a typo fails here rather than
 * issuing a request to a path the shell never meant to read.
 */
function requireNativePath(routeId: string): string {
  const route = findLoomNativeRoute(routeId);
  if (!route) {
    throw new LoomNativeRouteError(routeId);
  }
  return route.path;
}

/** Read liveness; the shell renders a degraded state on `backend_error`. */
export function shellHealthQueryOptions() {
  return queryOptions({
    queryKey: LOOM_SHELL_HEALTH_QUERY_KEY,
    queryFn: ({ signal }): Promise<LoomHealthResponse> =>
      loomNativeJson<LoomHealthResponse>(requireNativePath("loom.health"), {
        signal,
      }),
    staleTime: 30_000,
    retry: 1,
  });
}

/** Read the sidebar bootstrap and keep the offline replay cache warm. */
export function shellSidebarQueryOptions() {
  return queryOptions({
    queryKey: sidebarNavigationQueryKey(),
    queryFn: async ({ signal }): Promise<SidebarBootstrapResponse> => {
      const response = await loomApiJson<SidebarBootstrapResponse>(
        "projects.sidebarBootstrap",
        { signal },
      );
      writeCachedSidebarBootstrap(response);
      return response;
    },
    staleTime: Infinity,
    refetchOnWindowFocus: false,
    placeholderData: () => readCachedSidebarBootstrap() ?? undefined,
  });
}

/** Read system config (settings, keybindings, appearance, feature flags). */
export function shellSystemConfigQueryOptions() {
  return queryOptions({
    queryKey: systemConfigQueryKey(),
    queryFn: ({ signal }): Promise<SystemConfigResponse> =>
      loomApiJson<SystemConfigResponse>("system.config", { signal }),
    staleTime: 60_000,
    refetchOnWindowFocus: false,
  });
}

export function useShellHealth() {
  return useQuery(shellHealthQueryOptions());
}

export function useShellSidebarBootstrap() {
  return useQuery(shellSidebarQueryOptions());
}

export function useShellSystemConfig() {
  return useQuery(shellSystemConfigQueryOptions());
}
