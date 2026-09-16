import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import {
  shellHealthQueryOptions,
  shellSidebarQueryOptions,
  shellSystemConfigQueryOptions,
  useShellHealth,
  useShellSidebarBootstrap,
  useShellSystemConfig,
} from "@/lib/loom-shell";
import {
  sidebarNavigationQueryKey,
  systemConfigQueryKey,
} from "@/hooks/queries/query-keys";
import { resetSidebarBootstrapCacheForTest } from "@/lib/sidebar-bootstrap-cache";

function renderQuery<T>(useHook: () => { data: T | undefined; isPending: boolean; isError: boolean; error: unknown }) {
  function Probe() {
    const query = useHook();
    if (query.isPending) return <p>loading</p>;
    if (query.isError) return <p>error: {(query.error as Error).message}</p>;
    return <p>data: {JSON.stringify(query.data)}</p>;
  }
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });
  return render(
    <QueryClientProvider client={client}>
      <Probe />
    </QueryClientProvider>,
  );
}

const SIDEBAR_BODY = {
  sections: [],
  projects: [],
  personalProject: {
    id: "proj_personal",
    kind: "personal",
    name: "Personal",
    gitRemoteUrl: null,
    createdAt: 0,
    updatedAt: 0,
    sources: [],
    threads: [],
    defaultExecutionOptions: null,
  },
};

const SYSTEM_CONFIG_BODY = { generalSettings: {}, serverUrl: "http://localhost" };

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
  resetSidebarBootstrapCacheForTest();
});

describe("loom shell bootstrap", () => {
  it("shows loading then the sidebar bootstrap from a same-origin request", async () => {
    const fetchMock = vi.fn(async (input: RequestInfo | URL) => {
      expect(String(input)).toContain("/api/v1/sidebar-bootstrap");
      return new Response(JSON.stringify(SIDEBAR_BODY), {
        status: 200,
        headers: { "content-type": "application/json" },
      });
    });
    vi.stubGlobal("fetch", fetchMock);

    renderQuery(useShellSidebarBootstrap);

    expect(screen.getByText("loading")).toBeDefined();
    await waitFor(() => expect(screen.getByText(/proj_personal/u)).toBeDefined());
    expect(fetchMock).toHaveBeenCalledTimes(1);
  });

  it("reports an HTTP error instead of fabricating sidebar data", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(
        async () =>
          new Response(JSON.stringify({ code: "boom", message: "no backend" }), {
            status: 500,
            headers: { "content-type": "application/json" },
          }),
      ),
    );

    renderQuery(useShellSidebarBootstrap);

    await waitFor(() =>
      expect(screen.getByText(/HTTP 500: no backend/u)).toBeDefined(),
    );
  });

  it("reads health from the loom-native route and surfaces a degraded backend", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: RequestInfo | URL) => {
        expect(String(input)).toContain("/health");
        return new Response(
          JSON.stringify({
            status: "ok",
            protocol_version: 1,
            node_id: "node-1",
            uptime_ms: 5,
            readers: 2,
            retained_events: 3,
            backend_error: "redis unavailable",
          }),
          { status: 200, headers: { "content-type": "application/json" } },
        );
      }),
    );

    renderQuery(useShellHealth);

    await waitFor(() =>
      expect(screen.getByText(/redis unavailable/u)).toBeDefined(),
    );
  });

  it("reads system config through the typed contract route", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: RequestInfo | URL) => {
        const url = String(input);
        expect(url).toContain("/api/v1/system/config");
        expect(url.startsWith("http://localhost")).toBe(true);
        return new Response(JSON.stringify(SYSTEM_CONFIG_BODY), {
          status: 200,
          headers: { "content-type": "application/json" },
        });
      }),
    );

    renderQuery(useShellSystemConfig);

    await waitFor(() =>
      expect(screen.getByText(/localhost/u)).toBeDefined(),
    );
  });

  it("shares the ported query keys so cache ownership cannot diverge", () => {
    // The shell must not invent parallel keys: if it did, "the shell loaded"
    // and "the app has data" would be two different cache entries, and a
    // sidebar invalidation would leave one of them stale.
    expect(shellSidebarQueryOptions().queryKey).toEqual(
      sidebarNavigationQueryKey(),
    );
    expect(shellSystemConfigQueryOptions().queryKey).toEqual(
      systemConfigQueryKey(),
    );
    expect(shellHealthQueryOptions().queryKey).toEqual([
      "loom",
      "shell",
      "health",
    ]);
  });

  it("keeps health as the only shell-owned key", () => {
    const sidebarKey = shellSidebarQueryOptions().queryKey;
    const configKey = shellSystemConfigQueryOptions().queryKey;
    const healthKey = shellHealthQueryOptions().queryKey;
    expect(sidebarKey).not.toEqual(healthKey);
    expect(configKey).not.toEqual(healthKey);
    expect(sidebarKey).not.toEqual(configKey);
  });
});
