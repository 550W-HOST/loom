import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, cleanup, render, screen, waitFor } from "@testing-library/react";
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

function ok(body: unknown) {
  return new Response(JSON.stringify(body), {
    status: 200,
    headers: { "content-type": "application/json" },
  });
}

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

  it("keeps the response in memory for replay as soon as the read succeeds", async () => {
    window.localStorage.clear();
    vi.stubGlobal(
      "fetch",
      vi.fn(async () =>
        ok({
          ...SIDEBAR_BODY,
          projects: [
            {
              id: "proj_1",
              kind: "standard",
              name: "One",
              gitRemoteUrl: null,
              createdAt: 0,
              updatedAt: 0,
              sources: [],
              threads: [],
              defaultExecutionOptions: null,
            },
          ],
        }),
      ),
    );

    renderQuery(useShellSidebarBootstrap);
    await waitFor(() => expect(screen.getByText(/proj_1/u)).toBeDefined());

    // The in-memory replay value is set synchronously with the read; only the
    // localStorage write is deferred. Asserting this separately keeps the
    // assertion independent of the deferral timer.
    const { readCachedSidebarBootstrap } = await import(
      "@/lib/sidebar-bootstrap-cache"
    );
    expect(readCachedSidebarBootstrap()?.projects[0]?.id).toBe("proj_1");
  });

  it("persists the bounded sidebar cache off the critical path", async () => {
    // The write is deferred (requestIdleCallback, falling back to a 1s timer),
    // so it is driven explicitly here. A `waitFor` on localStorage would race
    // that 1s deferral against waitFor's own 1s default deadline, which is
    // exactly how this test was intermittently red in CI.
    vi.useFakeTimers({ shouldAdvanceTime: true });
    try {
      const { SIDEBAR_BOOTSTRAP_CACHE_KEY } = await import(
        "@/lib/sidebar-bootstrap-cache"
      );
      window.localStorage.clear();
      vi.stubGlobal(
        "fetch",
        vi.fn(async () =>
          ok({
            ...SIDEBAR_BODY,
            projects: [
              {
                id: "proj_1",
                kind: "standard",
                name: "One",
                gitRemoteUrl: null,
                createdAt: 0,
                updatedAt: 0,
                sources: [],
                threads: [],
                defaultExecutionOptions: null,
              },
            ],
          }),
        ),
      );

      renderQuery(useShellSidebarBootstrap);
      await waitFor(() => expect(screen.getByText(/proj_1/u)).toBeDefined());

      // Nothing is written before the deferral fires.
      expect(
        window.localStorage.getItem(SIDEBAR_BOOTSTRAP_CACHE_KEY),
      ).toBeNull();

      await act(async () => {
        await vi.advanceTimersByTimeAsync(5_000);
      });

      const stored = window.localStorage.getItem(SIDEBAR_BOOTSTRAP_CACHE_KEY);
      expect(stored).not.toBeNull();
      expect(stored).toContain("proj_1");
    } finally {
      vi.useRealTimers();
    }
  });

  it("does not invent sidebar data when the request fails", async () => {
    window.localStorage.clear();
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => {
        throw new TypeError("Failed to fetch");
      }),
    );

    renderQuery(useShellSidebarBootstrap);

    await waitFor(() => expect(screen.getByText(/error:/u)).toBeDefined());
    const { SIDEBAR_BOOTSTRAP_CACHE_KEY } = await import(
      "@/lib/sidebar-bootstrap-cache"
    );
    expect(window.localStorage.getItem(SIDEBAR_BOOTSTRAP_CACHE_KEY)).toBeNull();
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
