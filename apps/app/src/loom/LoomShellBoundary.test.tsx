import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { LoomShellBoundary } from "./LoomShellBoundary";
import { useShellSidebarBootstrap } from "@/lib/loom-shell";
import { resetSidebarBootstrapCacheForTest } from "@/lib/sidebar-bootstrap-cache";

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

const HEALTH_BODY = {
  status: "ok",
  protocol_version: 1,
  node_id: "node-1",
  uptime_ms: 1,
  readers: 1,
  retained_events: 0,
};

const CONFIG_BODY = { generalSettings: {}, serverUrl: "http://localhost" };

function ok(body: unknown) {
  return jsonResponse(body, 200);
}

function jsonResponse(body: unknown, status: number): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  });
}

function renderBoundary() {
  const client = new QueryClient({
    // `retryDelay: 0` keeps a retried request immediate. The shell's health
    // probe retries once, and React Query's default 1s backoff would otherwise
    // race `waitFor`'s 1s deadline — the same timing trap as the sidebar-cache
    // test. Retrying is a product decision; the delay is a test-harness
    // decision.
    defaultOptions: { queries: { retry: false, retryDelay: 0 } },
  });
  return render(
    <QueryClientProvider client={client}>
      <LoomShellBoundary>
        <p>workspace</p>
      </LoomShellBoundary>
    </QueryClientProvider>,
  );
}

/** Put a real sidebar value in the replay cache, as a previous visit would. */
async function seedSidebarCache(): Promise<void> {
  resetSidebarBootstrapCacheForTest();
  vi.stubGlobal(
    "fetch",
    vi.fn(async (input: RequestInfo | URL) => {
      const url = String(input);
      if (url.includes("/health")) return ok(HEALTH_BODY);
      if (url.includes("/sidebar-bootstrap")) return ok(SIDEBAR_BODY);
      return ok(CONFIG_BODY);
    }),
  );
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false, retryDelay: 0, gcTime: 0 } },
  });
  function Seed() {
    const sidebar = useShellSidebarBootstrap();
    return <span>{sidebar.data ? "seeded" : "seeding"}</span>;
  }
  const view = render(
    <QueryClientProvider client={client}>
      <Seed />
    </QueryClientProvider>,
  );
  await waitFor(() => expect(screen.getByText("seeded")).toBeDefined());
  view.unmount();
  client.clear();
}

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
  resetSidebarBootstrapCacheForTest();
});

describe("LoomShellBoundary", () => {
  it("renders the shell only after liveness and system config resolve", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: RequestInfo | URL) => {
        const url = String(input);
        if (url.includes("/health")) return ok(HEALTH_BODY);
        if (url.includes("/sidebar-bootstrap")) return ok(SIDEBAR_BODY);
        return ok(CONFIG_BODY);
      }),
    );

    renderBoundary();

    expect(screen.getByTestId("route-loading-skeleton")).toBeDefined();
    await waitFor(() => expect(screen.getByText("workspace")).toBeDefined());
  });

  it("renders an explicit unreachable state instead of an empty sidebar", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => {
        throw new TypeError("Failed to fetch");
      }),
    );

    renderBoundary();

    await waitFor(() => expect(screen.getByTestId("loom-shell-error")).toBeDefined());
    expect(screen.getByText("Cannot reach the loom server")).toBeDefined();
    expect(screen.queryByText("workspace")).toBeNull();
  });

  it("reports a health failure even when the sidebar has cached data", async () => {
    // The sidebar has an offline replay cache, so "we have sidebar data" says
    // nothing about liveness. A cached sidebar must not mask a dead server.
    await seedSidebarCache();
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: RequestInfo | URL) => {
        const url = String(input);
        if (url.includes("/health")) throw new TypeError("Failed to fetch");
        if (url.includes("/sidebar-bootstrap")) return ok(SIDEBAR_BODY);
        return ok(CONFIG_BODY);
      }),
    );

    renderBoundary();

    await waitFor(() => expect(screen.getByTestId("loom-shell-error")).toBeDefined());
    expect(screen.queryByText("workspace")).toBeNull();
  });

  it("reports a system-config failure even when the sidebar has cached data", async () => {
    await seedSidebarCache();
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: RequestInfo | URL) => {
        const url = String(input);
        if (url.includes("/health")) return ok(HEALTH_BODY);
        if (url.includes("/sidebar-bootstrap")) return ok(SIDEBAR_BODY);
        return jsonResponse({ code: "boom", message: "no config" }, 500);
      }),
    );

    renderBoundary();

    await waitFor(() =>
      expect(screen.getByText("Cannot load server settings")).toBeDefined(),
    );
    expect(screen.queryByText("workspace")).toBeNull();
  });

  it("stays rendered when health reports a degraded backend and warns persistently", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: RequestInfo | URL) => {
        const url = String(input);
        if (url.includes("/health")) {
          return ok({ ...HEALTH_BODY, backend_error: "disk full" });
        }
        if (url.includes("/sidebar-bootstrap")) return ok(SIDEBAR_BODY);
        return ok(CONFIG_BODY);
      }),
    );

    renderBoundary();

    await waitFor(() => expect(screen.getByText("workspace")).toBeDefined());
    // The warning is a persistent banner, not a transient toast, and it does
    // not block the app: degraded durability still serves reads.
    const banner = screen.getByTestId("loom-shell-degraded");
    expect(banner.textContent).toContain("disk full");
    expect(banner.textContent).toContain("Storage is degraded");
    expect(screen.queryByTestId("loom-shell-error")).toBeNull();
  });

  it("shows a stale-data notice when the sidebar read fails but a cache exists", async () => {
    await seedSidebarCache();
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: RequestInfo | URL) => {
        const url = String(input);
        if (url.includes("/health")) return ok(HEALTH_BODY);
        if (url.includes("/sidebar-bootstrap")) {
          throw new TypeError("Failed to fetch");
        }
        return ok(CONFIG_BODY);
      }),
    );

    renderBoundary();

    await waitFor(() =>
      expect(screen.getByTestId("loom-shell-stale")).toBeDefined(),
    );
    // The app still renders: the failure is disclosed rather than hidden.
    expect(screen.getByText("workspace")).toBeDefined();
  });
});
