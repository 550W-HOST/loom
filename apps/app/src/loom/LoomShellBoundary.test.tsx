import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { LoomShellBoundary } from "./LoomShellBoundary";
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
  return new Response(JSON.stringify(body), {
    status: 200,
    headers: { "content-type": "application/json" },
  });
}

function renderBoundary() {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={client}>
      <LoomShellBoundary>
        <p>workspace</p>
      </LoomShellBoundary>
    </QueryClientProvider>,
  );
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

  it("stays rendered when health reports a degraded backend", async () => {
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
    expect(screen.queryByTestId("loom-shell-error")).toBeNull();
  });
});
